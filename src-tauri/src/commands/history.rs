//! 命令历史命令（终端智能补全的数据源）。
//!
//! 历史在终端输入回车时记录（前端从屏幕缓冲提取命令文本），存入 SQLite
//! `history` 表（见 [`crate::storage::history_repo`]）。补全建议读取全局
//! 最近历史（跨会话、按命令去重），与 shell 的 Ctrl+R 体验一致。

use tauri::State;

use crate::error::{AppError, AppResult};
use crate::state::AppState;
use crate::storage::history_repo::{self, HistoryEntry};

/// 单条命令的最大记录长度（超长命令不记录——多为粘贴的脚本/密钥，
/// 对补全无价值且有隐私风险）。前端已有同款过滤，这里双保险。
const MAX_COMMAND_LEN: usize = 512;

/// 新增一条命令历史（同命令只保留最新一次执行，自动去重）。
#[tauri::command]
pub fn history_add(
    session_id: String,
    command: String,
    state: State<'_, AppState>,
) -> AppResult<i64> {
    let cmd = command.trim();
    if cmd.is_empty() {
        return Err(AppError::InvalidInput("命令不能为空".into()));
    }
    if cmd.chars().count() > MAX_COMMAND_LEN {
        return Ok(0);
    }
    let conn = state.conn()?;
    let entry = HistoryEntry {
        id: None,
        session_id,
        command: cmd.to_string(),
        exit_code: None,
        run_at: chrono::Local::now().to_rfc3339(),
    };
    history_repo::add_history_dedup(&conn, &entry)
}

/// 全局最近历史（按命令去重，新→旧），用于终端补全建议。
#[tauri::command]
pub fn history_recent(limit: u32, state: State<'_, AppState>) -> AppResult<Vec<HistoryEntry>> {
    let conn = state.conn()?;
    history_repo::list_recent_history(&conn, limit.clamp(1, 1000))
}

/// 删除一条历史（补全弹窗的删除按钮）。
#[tauri::command]
pub fn history_delete(id: i64, state: State<'_, AppState>) -> AppResult<()> {
    let conn = state.conn()?;
    history_repo::delete_history(&conn, id)
}

/// 关键字模糊搜索历史（所有会话范围）。
#[tauri::command]
pub fn history_search(
    keyword: String,
    limit: u32,
    state: State<'_, AppState>,
) -> AppResult<Vec<HistoryEntry>> {
    let conn = state.conn()?;
    history_repo::search_history(&conn, &keyword, limit.clamp(1, 1000))
}
