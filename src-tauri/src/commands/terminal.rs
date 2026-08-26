//! 终端输入与窗口控制命令。
//!
//! 终端实例的输出由 reader 任务以事件推送（见 [`crate::events::TERMINAL_DATA`]），
//! 这里只处理前端 → 远程方向：写入键盘输入、调整窗口大小。

use base64::Engine;
use tauri::State;

use crate::error::{AppError, AppResult};
use crate::state::AppState;

/// 等待一次终端写入 ack 的最长等待时间。
///
/// reader 侧退出（会话关闭）或底层连接卡死时 ack 可能永远不来，ZMODEM 上传
/// 等背压路径会永久挂起；超时兜底并给出明确错误。
const TERMINAL_WRITE_ACK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 向指定终端实例写入数据（前端键盘输入 / ZMODEM 协议字节）。
///
/// `data` 是 base64 编码的字节流（与输出方向保持一致，便于传输二进制控制字符）。
///
/// 异步 + 写确认：等待 reader 把字节真正写入 SSH channel 后才返回，
/// 为 ZMODEM 大文件上传提供背压（防止无界输入队列堆积整个文件）。
#[tauri::command]
pub async fn terminal_write(
    instance_id: String,
    data: String,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.as_bytes())
        .map_err(|e| AppError::InvalidInput(format!("base64 解码失败: {}", e)))?;

    // 短临界区：只取出 ack receiver 即释放锁，再在锁外等待写完成。
    let ack_rx = {
        let terminals = state.terminals.lock();
        let session = terminals
            .get(&instance_id)
            .ok_or_else(|| AppError::NotFound(format!("终端 {} 不存在", instance_id)))?;
        session.write_with_ack(bytes)?
    };

    match tokio::time::timeout(TERMINAL_WRITE_ACK_TIMEOUT, ack_rx).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(_)) => Err(AppError::Ssh("终端 reader 已退出".into())),
        Err(_) => Err(AppError::Ssh(format!(
            "写入终端超时（{} 秒），连接可能已卡死",
            TERMINAL_WRITE_ACK_TIMEOUT.as_secs()
        ))),
    }
}

/// 调整指定终端实例的窗口大小。
#[tauri::command]
pub fn terminal_resize(
    instance_id: String,
    cols: u32,
    rows: u32,
    state: State<'_, AppState>,
) -> AppResult<()> {
    let terminals = state.terminals.lock();
    let session = terminals
        .get(&instance_id)
        .ok_or_else(|| AppError::NotFound(format!("终端 {} 不存在", instance_id)))?;
    session.resize(cols, rows)
}

/// terminal_attach 的返回：缓冲内容 + 累计字节基线。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalAttachResult {
    /// 输出环形缓冲的原始字节（base64）。
    pub data: String,
    /// 快照对应的累计输出字节数（单调计数，含已被环形截断的头部）。
    /// 前端把它作为去重基线：terminal:data 事件里 `total <= 基线` 的块
    /// 已包含在快照中，直接跳过。
    pub total: usize,
}

/// 取终端输出的缓冲快照（前端挂载/重绑时的 attach 回放）。
///
/// 后端 reader 在 connect 命令返回前就开始 emit `terminal:data`，而前端要等
/// invoke 返回 → 渲染 → `listen()` 注册完成才能收到事件——窗口期内的输出
/// （SSH banner/MOTD、telnet 登录提示、本地 shell 提示符）会整体丢失，表现
/// 为终端开出来是空白屏。前端在注册监听**之后**调用本命令：
/// 1. 先把监听期收到的事件缓存起来（不渲染）；
/// 2. 本命令先冲刷 reader 的输出批次（SSH/Telnet 批量 emit），再原子返回
///    「缓冲快照 + 累计基线」（同一锁内读取），保证快照与事件流的 total 一致；
/// 3. 写入快照，再按事件的 `total` 字段过滤缓存（≤ 基线的已含在快照里），
///    既不丢字节也不重复渲染。
#[tauri::command]
pub async fn terminal_attach(
    instance_id: String,
    state: State<'_, AppState>,
) -> AppResult<TerminalAttachResult> {
    // 先冲刷输出批次（async 等待 reader 处理，不持有 std Mutex 跨 await）。
    let flush_rx = {
        let terminals = state.terminals.lock();
        match terminals.get(&instance_id) {
            Some(s) => s.flush_output(),
            None => return Err(AppError::NotFound(format!("终端 {} 不存在", instance_id))),
        }
    };
    if let Some(rx) = flush_rx {
        let _ = tokio::time::timeout(std::time::Duration::from_millis(200), rx).await;
    }

    let terminals = state.terminals.lock();
    let session = terminals
        .get(&instance_id)
        .ok_or_else(|| AppError::NotFound(format!("终端 {} 不存在", instance_id)))?;
    let (bytes, total) = session.attach_snapshot();
    Ok(TerminalAttachResult {
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        total,
    })
}

/// 取终端最近输出的文本快照（用于"终端上下文感知"）。
///
/// `max_bytes` 限制返回字节数，0 表示默认（8 KiB）。
#[tauri::command]
pub fn terminal_snapshot(
    instance_id: String,
    max_bytes: usize,
    state: State<'_, AppState>,
) -> AppResult<String> {
    let terminals = state.terminals.lock();
    let session = terminals
        .get(&instance_id)
        .ok_or_else(|| AppError::NotFound(format!("终端 {} 不存在", instance_id)))?;
    let snap = match session {
        crate::state::TerminalSession::Ssh(s) => s.snapshot(max_bytes),
        crate::state::TerminalSession::Telnet(s) => s.snapshot(max_bytes),
        crate::state::TerminalSession::Local(s) => s.snapshot(max_bytes),
    };
    Ok(snap)
}
