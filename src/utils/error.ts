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
