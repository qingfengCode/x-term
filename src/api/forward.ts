import { invoke } from "@tauri-apps/api/core";
import type { ForwardRule } from "./types";

export function forwardListRules(): Promise<ForwardRule[]> {
  return invoke<ForwardRule[]>("forward_list_rules");
}

/** 保存转发规则，返回落库后的完整规则（含最终 id，后端可能规范化客户端 id）。 */
export function forwardSaveRule(rule: ForwardRule): Promise<ForwardRule> {
  return invoke<ForwardRule>("forward_save_rule", { rule });
}

export function forwardDeleteRule(id: string): Promise<void> {
  return invoke<void>("forward_delete_rule", { id });
}

export function forwardStart(ruleId: string): Promise<string> {
  return invoke<string>("forward_start", { ruleId });
}

export function forwardStop(ruleId: string): Promise<void> {
  return invoke<void>("forward_stop", { ruleId });
}

/** 当前正在运行的转发规则 id 列表（后端持有真实状态）。 */
export function forwardListRunning(): Promise<string[]> {
  return invoke<string[]>("forward_list_running");
}
