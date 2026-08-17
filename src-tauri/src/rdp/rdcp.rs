//! RDCleanPath PDU 编解码（Devolutions Gateway 的 WebSocket 握手协议）。
//!
//! IronRDP 官方 web 客户端（WASM）不直连 RDP TCP，而是先通过 WebSocket 向
//! 「代理」（官方为 Devolutions Gateway）发送一个 DER 编码的 RDCleanPath 请求，
//! 由代理完成 TCP 建连与 X.224/TLS 握手并回传证书链，之后双方在该 WebSocket
//! 上直接跑 RDP 字节流。
//!
//! 本模块镜像 IronRDP 仓库 `ironrdp-rdcleanpath` crate 的 ASN.1 结构
//! （DER、全部字段 EXPLICIT 包裹），使我们的桥接能被官方 WASM 客户端
//! 直接连接。结构定义以 <https://github.com/Devolutions/IronRDP> 为准。

use der::asn1::OctetString;
use der::Sequence;

/// 协议基础版本（即 RDP 端口号）。
pub const BASE_VERSION: u64 = 3389;
/// 当前包版本（官方常量 VERSION_1）。
pub const VERSION_1: u64 = BASE_VERSION + 1;

/// 一般错误码（可附带 http_status / wsa_last_error / tls_alert 细节）。
pub const GENERAL_ERROR_CODE: u16 = 1;
/// 协商错误码（X.224 协商失败，包体同时携带服务器返回的 X.224 数据）。
pub const NEGOTIATION_ERROR_CODE: u16 = 2;

/// RDCleanPath 包（代理↔客户端握手与错误信令）。
#[derive(Clone, Debug, Default, Eq, PartialEq, Sequence)]
#[asn1(tag_mode = "EXPLICIT")]
pub struct RDCleanPathPdu {
    /// 包版本（3390）。
    #[asn1(context_specific = "0")]
    pub version: u64,
    /// 代理返回的错误（仅代理→客户端）。
    #[asn1(context_specific = "1", optional = "true")]
    pub error: Option<RDCleanPathErr>,
    /// 目标 RDP 服务器地址（仅客户端→代理）。
    #[asn1(context_specific = "2", optional = "true")]
    pub destination: Option<String>,
    /// 代理鉴权 token（官方为 JET token；本桥接不校验）。
    #[asn1(context_specific = "3", optional = "true")]
    pub proxy_auth: Option<String>,
    /// 服务器侧鉴权（当前未使用）。
    #[asn1(context_specific = "4", optional = "true")]
    pub server_auth: Option<String>,
    /// 预连接数据（PCB），代理原样转发给 RDP 服务器。
    #[asn1(context_specific = "5", optional = "true")]
    pub preconnection_blob: Option<String>,
    /// X.224 连接握手数据（请求或确认）。
    #[asn1(context_specific = "6", optional = "true")]
    pub x224_connection_pdu: Option<OctetString>,
    /// RDP 服务器 TLS 证书链（仅代理→客户端）。
    #[asn1(context_specific = "7", optional = "true")]
    pub server_cert_chain: Option<Vec<OctetString>>,
    /// 代理解析出的服务器地址（客户端忽略）。
    #[asn1(context_specific = "9", optional = "true")]
    pub server_addr: Option<String>,
}

/// RDCleanPath 错误详情。
#[derive(Clone, Debug, Default, Eq, PartialEq, Sequence)]
#[asn1(tag_mode = "EXPLICIT")]
pub struct RDCleanPathErr {
    #[asn1(context_specific = "0")]
    pub error_code: u16,
    #[asn1(context_specific = "1", optional = "true")]
    pub http_status_code: Option<u16>,
    #[asn1(context_specific = "2", optional = "true")]
    pub wsa_last_error: Option<u16>,
    #[asn1(context_specific = "3", optional = "true")]
    pub tls_alert_code: Option<u8>,
}

impl RDCleanPathPdu {
    /// 构造握手成功响应：携带服务器 X.224 确认、TLS 证书链与解析出的服务器地址。
    pub fn new_response(
        server_addr: String,
        x224_pdu: Vec<u8>,
        x509_chain: Vec<Vec<u8>>,
    ) -> der::Result<Self> {
        Ok(Self {
            version: VERSION_1,
            x224_connection_pdu: Some(OctetString::new(x224_pdu)?),
            server_cert_chain: Some(
                x509_chain
                    .into_iter()
                    .map(OctetString::new)
                    .collect::<der::Result<_>>()?,
            ),
            server_addr: Some(server_addr),
            ..Self::default()
        })
    }

    /// 构造带 HTTP 状态码的一般错误（对应官方 `new_http_error`）。
    pub fn new_http_error(status_code: u16) -> Self {
        Self {
            version: VERSION_1,
            error: Some(RDCleanPathErr {
                error_code: GENERAL_ERROR_CODE,
                http_status_code: Some(status_code),
                ..Default::default()
            }),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use der::{Decode, Encode};

    use super::*;

    /// 伪造一段 X.224 Connection Request 字节（仅做编解码载体，无需合法）。
    fn fake_x224(len: usize) -> Vec<u8> {
        vec![0x5a; len]
    }

    #[test]
    fn request_roundtrip() {
        // 模拟客户端发来的请求包：destination + proxy_auth + pcb + x224。
        let request = RDCleanPathPdu {
            version: VERSION_1,
            destination: Some("10.10.0.3:3389".into()),
            proxy_auth: Some(String::new()),
            preconnection_blob: Some("pcb-bytes".into()),
            x224_connection_pdu: Some(OctetString::new(fake_x224(19)).unwrap()),
            ..Default::default()
        };

        let der = request.to_der().unwrap();
        // DER 顶层必须是 SEQUENCE（0x30）。
        assert_eq!(der[0], 0x30);

        let decoded = RDCleanPathPdu::from_der(&der).unwrap();
        assert_eq!(decoded, request);
        assert_eq!(decoded.version, VERSION_1);
        assert_eq!(decoded.destination.as_deref(), Some("10.10.0.3:3389"));
        assert_eq!(
            decoded.x224_connection_pdu.unwrap().as_bytes(),
            &fake_x224(19)
        );
    }

    #[test]
    fn response_roundtrip() {
        let response = RDCleanPathPdu::new_response(
            "10.10.0.3".into(),
            fake_x224(11),
            vec![vec![1, 2, 3], vec![4, 5, 6]],
        )
        .unwrap();

        let der = response.to_der().unwrap();
        let decoded = RDCleanPathPdu::from_der(&der).unwrap();
        assert_eq!(decoded, response);

        let chain = decoded.server_cert_chain.unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].as_bytes(), &[1, 2, 3]);
        assert_eq!(chain[1].as_bytes(), &[4, 5, 6]);
        assert_eq!(decoded.server_addr.as_deref(), Some("10.10.0.3"));
    }

    #[test]
    fn http_error_roundtrip() {
        let error = RDCleanPathPdu::new_http_error(502);
        let der = error.to_der().unwrap();
        let decoded = RDCleanPathPdu::from_der(&der).unwrap();
        assert_eq!(decoded, error);

        let err = decoded.error.unwrap();
        assert_eq!(err.error_code, GENERAL_ERROR_CODE);
        assert_eq!(err.http_status_code, Some(502));
    }
}
