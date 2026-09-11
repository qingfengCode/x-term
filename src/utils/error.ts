/**
 * 判断错误是否为 SSH 认证类错误。
 *
 * 后端 `AppError::Auth` 序列化为以「认证错误:」开头的字符串（如
 * 「认证错误: SSH 认证失败: user@host:22」）。此类错误通常意味着密码错误、
 * 服务器要求口令码/验证码等二次认证，前端应弹出手动认证框让用户重试。
 */
export function isAuthError(e: unknown): boolean {
  return String(e).includes("认证错误");
}

/** 任意抛出值 → 可读错误消息（Error 取 message，其余字符串化）。
 * 供各处 `catch` 统一提取，替代 `catch (e: any)` + `e?.message`。 */
export function errorMessage(e: unknown): string {
  return e instanceof Error ? e.message : String(e);
}

/**
 * 判断错误是否为「用户主动取消了认证输入」（后端文本「二次认证已取消」）。
 *
 * 用户在二次认证弹窗点「取消」= 明确放弃本次连接：调用方不应再自动弹
 * 重试框（取消后立即再弹一个对话框，两个 append-to-body 的 Dialog 快速
 * 关/开会让 Element Plus 遮罩/焦点陷阱清理交错残留，导致后续页面点击
 * 失效），应直接清理占位 tab 恢复原状。
 */
export function isAuthCancelled(e: unknown): boolean {
  // 用完整锚点串而非裸"已取消"：避免误吞其它含"已取消"的无关错误
  //（后端文案为「二次认证已取消: user@host:port」，见 ssh/client.rs）。
  return String(e).includes("二次认证已取消");
}
