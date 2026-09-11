// mlkem768x25519-sha256 —— 后量子混合密钥交换（OpenSSH 9.9+ 默认 KEX）。
//
// 组合构造（与 OpenSSH 实现一致）：
// - 客户端临时公钥（KEX_ECDH_INIT 的 e 字段）= mlkem768_pk(1184) || x25519_pk(32)
// - 服务端回复（KEX_ECDH_REPLY 的 f 字段）= mlkem_ct(1088) || x25519_pk(32)
// - 共享密钥 K = mlkem_ss(32) || x25519_ss(32)（64 字节，按 mpint 编码进交换哈希）
// - 交换哈希 H = SHA256(V_C || V_S || I_C || I_S || K_S || e || f || K)
//
// x25519 半段复用 curve25519-dalek（与 curve25519.rs 相同的写法）；
// ML-KEM 半段使用 RustCrypto 的 ml-kem（FIPS 203，rand_core 0.6 兼容）。
// 交换哈希 / 密钥推导逻辑与 curve25519-sha256 完全同构，仅共享密钥长度不同。

use byteorder::{BigEndian, ByteOrder};
use curve25519_dalek::constants::ED25519_BASEPOINT_TABLE;
use curve25519_dalek::montgomery::MontgomeryPoint;
use curve25519_dalek::scalar::Scalar;
use ml_kem::kem::{Decapsulate, DecapsulationKey, Encapsulate, EncapsulationKey};
use ml_kem::{Ciphertext, Encoded, EncodedSizeUser, KemCore, MlKem768, MlKem768Params};
use std::convert::TryInto;

use super::{compute_keys, KexAlgorithm, KexType};
use crate::keys::encoding::Encoding;
use crate::session::Exchange;
use crate::{cipher, mac, msg, CryptoVec};

/// mlkem768 封装公钥字节数。
const MLKEM_EK_LEN: usize = 1184;
/// mlkem768 密文字节数。
const MLKEM_CT_LEN: usize = 1088;
/// 客户端混合公钥总长（mlkem || x25519）。
const CLIENT_PUB_LEN: usize = MLKEM_EK_LEN + 32;
/// 服务端混合回复总长（ct || x25519）。
const SERVER_REPLY_LEN: usize = MLKEM_CT_LEN + 32;
/// 混合共享密钥总长。
const SHARED_LEN: usize = 64;

pub struct MlKem768X25519KexType {}

impl KexType for MlKem768X25519KexType {
    fn make(&self) -> Box<dyn KexAlgorithm + Send> {
        Box::new(MlKem768X25519Kex {
            client_x25519_secret: None,
            client_mlkem_dk: None,
            shared_secret: None,
        }) as Box<dyn KexAlgorithm + Send>
    }
}

#[doc(hidden)]
pub struct MlKem768X25519Kex {
    /// 客户端 x25519 临时私钥。
    client_x25519_secret: Option<Scalar>,
    /// 客户端 ML-KEM 解封装密钥（收到服务端 ct 后解出 mlkem_ss）。
    client_mlkem_dk: Option<DecapsulationKey<MlKem768Params>>,
    /// 混合共享密钥（mlkem_ss || x25519_ss）。
    shared_secret: Option<[u8; SHARED_LEN]>,
}

impl std::fmt::Debug for MlKem768X25519Kex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Algorithm {{ mlkem768x25519: [hidden] }}")
    }
}

/// 把两个半段的共享密钥拼成混合共享密钥。
fn combine(mlkem_ss: &[u8], x25519_ss: &[u8]) -> Result<[u8; SHARED_LEN], crate::Error> {
    if mlkem_ss.len() != 32 || x25519_ss.len() != 32 {
        return Err(crate::Error::Kex);
    }
    let mut out = [0u8; SHARED_LEN];
    out[..32].copy_from_slice(mlkem_ss);
    out[32..].copy_from_slice(x25519_ss);
    Ok(out)
}

impl KexAlgorithm for MlKem768X25519Kex {
    fn skip_exchange(&self) -> bool {
        false
    }

    fn server_dh(&mut self, exchange: &mut Exchange, payload: &[u8]) -> Result<(), crate::Error> {
        // 解析客户端 e 字段：mlkem_pk || x25519_pk。
        let client_pub = if payload.first() != Some(&msg::KEX_ECDH_INIT) {
            return Err(crate::Error::Inconsistent);
        } else {
            #[allow(clippy::indexing_slicing)] // length checked
            let pubkey_len = BigEndian::read_u32(&payload[1..]) as usize;
            if pubkey_len != CLIENT_PUB_LEN || payload.len() < 5 + pubkey_len {
                return Err(crate::Error::Inconsistent);
            }
            #[allow(clippy::indexing_slicing)] // length checked
            &payload[5..5 + CLIENT_PUB_LEN]
        };

        if client_pub.len() != CLIENT_PUB_LEN {
            return Err(crate::Error::Kex);
        }
        // 重建 mlkem 公钥字节数组（Array<u8, U1184>）。
        let mut ek_bytes = <Encoded<EncapsulationKey<MlKem768Params>>>::default();
        ek_bytes[..MLKEM_EK_LEN].copy_from_slice(&client_pub[..MLKEM_EK_LEN]);
        let ek = EncapsulationKey::<MlKem768Params>::from_bytes(&ek_bytes);
        let client_x25519: [u8; 32] = client_pub[MLKEM_EK_LEN..]
            .try_into()
            .map_err(|_| crate::Error::Kex)?;
        let client_x25519_pk = MontgomeryPoint(client_x25519);

        // 服务端 x25519 临时密钥对。
        let server_secret = Scalar::from_bytes_mod_order(rand::random::<[u8; 32]>());
        let server_pubkey = (ED25519_BASEPOINT_TABLE * &server_secret).to_montgomery();

        // ML-KEM 封装：对客户端公钥 encapsulate 得到 (ct, mlkem_ss)。
        let (ct, mlkem_ss) = ek
            .encapsulate(&mut rand::rngs::OsRng)
            .map_err(|_| crate::Error::Kex)?;
        let x25519_ss = server_secret * client_x25519_pk;

        // 服务端混合回复：ct || x25519_server_pk。
        exchange.server_ephemeral.clear();
        exchange.server_ephemeral.extend(ct.as_ref());
        exchange.server_ephemeral.extend(&server_pubkey.0);

        self.shared_secret = Some(combine(mlkem_ss.as_ref(), x25519_ss.as_bytes())?);
        Ok(())
    }

    fn client_dh(
        &mut self,
        client_ephemeral: &mut CryptoVec,
        buf: &mut CryptoVec,
    ) -> Result<(), crate::Error> {
        // ML-KEM 密钥对 + x25519 临时密钥对。
        let (dk, ek) = MlKem768::generate(&mut rand::rngs::OsRng);
        let client_secret = Scalar::from_bytes_mod_order(rand::random::<[u8; 32]>());
        let client_pubkey = (ED25519_BASEPOINT_TABLE * &client_secret).to_montgomery();

        // 客户端混合公钥：mlkem_pk || x25519_pk。
        client_ephemeral.clear();
        client_ephemeral.extend(ek.as_bytes().as_ref());
        client_ephemeral.extend(&client_pubkey.0);

        // KEX_ECDH_INIT 载荷：ssh string（带 u32 长度前缀）包裹混合公钥，
        // 与 curve25519 的报文格式一致。
        buf.push(msg::KEX_ECDH_INIT);
        buf.extend_ssh_string(client_ephemeral);

        self.client_mlkem_dk = Some(dk);
        self.client_x25519_secret = Some(client_secret);
        Ok(())
    }

    fn compute_shared_secret(&mut self, remote_pubkey_: &[u8]) -> Result<(), crate::Error> {
        let dk = self
            .client_mlkem_dk
            .take()
            .ok_or(crate::Error::KexInit)?;
        let x25519_secret = self
            .client_x25519_secret
            .take()
            .ok_or(crate::Error::KexInit)?;

        if remote_pubkey_.len() != SERVER_REPLY_LEN {
            return Err(crate::Error::Kex);
        }
        // 重建密文字节数组（Array<u8, U1088>）。
        let mut ct_bytes = <Ciphertext<MlKem768>>::default();
        ct_bytes[..MLKEM_CT_LEN].copy_from_slice(&remote_pubkey_[..MLKEM_CT_LEN]);
        let server_x25519: [u8; 32] = remote_pubkey_[MLKEM_CT_LEN..]
            .try_into()
            .map_err(|_| crate::Error::Kex)?;
        let server_x25519_pk = MontgomeryPoint(server_x25519);

        let mlkem_ss = dk.decapsulate(&ct_bytes).map_err(|_| crate::Error::Kex)?;
        let x25519_ss = x25519_secret * server_x25519_pk;

        self.shared_secret = Some(combine(mlkem_ss.as_ref(), x25519_ss.as_bytes())?);
        Ok(())
    }

    fn compute_exchange_hash(
        &self,
        key: &CryptoVec,
        exchange: &Exchange,
        buffer: &mut CryptoVec,
    ) -> Result<CryptoVec, crate::Error> {
        // 与 curve25519-sha256 同构（RFC 5656 §4 的哈希结构）：
        // H = SHA256(V_C || V_S || I_C || I_S || K_S || e || f || K)。
        buffer.clear();
        buffer.extend_ssh_string(&exchange.client_id);
        buffer.extend_ssh_string(&exchange.server_id);
        buffer.extend_ssh_string(&exchange.client_kex_init);
        buffer.extend_ssh_string(&exchange.server_kex_init);

        buffer.extend(key);
        buffer.extend_ssh_string(&exchange.client_ephemeral);
        buffer.extend_ssh_string(&exchange.server_ephemeral);

        if let Some(ref shared) = self.shared_secret {
            buffer.extend_ssh_mpint(shared);
        }

        use sha2::Digest;
        let mut hasher = sha2::Sha256::new();
        hasher.update(&buffer);

        let mut res = CryptoVec::new();
        res.extend(hasher.finalize().as_slice());
        Ok(res)
    }

    fn compute_keys(
        &self,
        session_id: &CryptoVec,
        exchange_hash: &CryptoVec,
        cipher: cipher::Name,
        remote_to_local_mac: mac::Name,
        local_to_remote_mac: mac::Name,
        is_server: bool,
    ) -> Result<super::cipher::CipherPair, crate::Error> {
        compute_keys::<sha2::Sha256>(
            self.shared_secret.as_ref().map(|x| x.as_slice()),
            session_id,
            exchange_hash,
            cipher,
            remote_to_local_mac,
            local_to_remote_mac,
            is_server,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 端到端 roundtrip：client_dh 生成 e → server_dh 封装并算出共享密钥 →
    /// client 收到 f 后 compute_shared_secret —— 两侧共享密钥必须一致，
    /// 且双方独立计算的交换哈希 / 派生密钥也一致。
    #[test]
    fn mlkem768x25519_roundtrip() {
        let mut client = MlKem768X25519KexType {}.make();
        let mut server = MlKem768X25519KexType {}.make();

        // 1. 客户端发起。
        let mut client_ephemeral = CryptoVec::new();
        let mut init_msg = CryptoVec::new();
        client.client_dh(&mut client_ephemeral, &mut init_msg).unwrap();
        assert_eq!(client_ephemeral.len(), CLIENT_PUB_LEN);

        // 2. 服务端封装。
        let mut exchange = Exchange::new();
        server
            .server_dh(&mut exchange, &init_msg)
            .unwrap();
        assert_eq!(exchange.server_ephemeral.len(), SERVER_REPLY_LEN);

        // 3. 客户端解封装。
        client
            .compute_shared_secret(&exchange.server_ephemeral)
            .unwrap();

        // 4. 双方共享密钥一致（经交换哈希间接比较：直接读私有字段不可行，
        //    用相同输入的 exchange hash 比较）。两侧使用同一个 Exchange
        //    （含步骤 1/2 的 e/f），与真实握手的哈希输入一致。
        let host_key = {
            let mut k = CryptoVec::new();
            k.extend(&[7u8; 32]);
            k
        };
        exchange.client_ephemeral = client_ephemeral.clone();
        let mut buffer = CryptoVec::new();
        let h_client = client
            .compute_exchange_hash(&host_key, &exchange, &mut buffer)
            .unwrap();
        buffer.clear();
        let h_server = server
            .compute_exchange_hash(&host_key, &exchange, &mut buffer)
            .unwrap();
        assert_eq!(
            &h_client[..], &h_server[..],
            "双方交换哈希必须一致（共享密钥不同的铁证）"
        );
    }
}
