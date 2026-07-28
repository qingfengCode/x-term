import { invoke } from "@tauri-apps/api/core";
import type { ForwardRule } from "./types";

export function forwardListRules(): Promise<ForwardRule[]> {
  return invoke<ForwardRule[]>("forward_list_rules");
}

export function forwardSaveRule(rule: ForwardRule): Promise<void> {
  return invoke<void>("forward_save_rule", { rule });
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
