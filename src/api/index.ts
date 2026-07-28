// 后端命令调用的薄封装。每个文件按业务领域组织。
// 调用约定：参数名与 Rust #[tauri::command] 的参数名一致（驼峰），Tauri 会自动转换。

export * as vaultApi from "./vault";
export * as sessionApi from "./session";
export * as terminalApi from "./terminal";
export * as sftpApi from "./sftp";
export * as forwardApi from "./forward";
export * as aiApi from "./ai";
export * as dbApi from "./db";
export * as configApi from "./config";
export * from "./types";
