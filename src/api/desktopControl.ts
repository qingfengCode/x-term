import { invoke } from "@tauri-apps/api/core";

/**
 * 内嵌 RDP 桌面控制注册表 + AI 桌面工具（desktop_*）的前端执行体。
 *
 * IronRDP WASM 会话活在前端 RdpPane 组件里（后端桥接只透传字节），因此 AI 的
 * desktop_screenshot / desktop_click / desktop_type / desktop_key 必须由前端执行：
 * RdpPane 挂载时把控制句柄注册进来（键为桥接 instanceId），桌面助手（AiPanel
 * domain="desktop"）的工具执行器按 instanceId 取出句柄调用截图/点击/输入，
 * 再把「批准 + 执行」一体回执经 `ai_desktop_tool_respond` 命令回传后端编排循环。
 */

/** RdpPane 暴露给 AI 的桌面控制句柄。 */
export interface DesktopControl {
  /** 远端桌面分辨率（画布 backing 像素，截图与点击共用的坐标系）。未连接返回 null。 */
  size(): { width: number; height: number } | null;
  /** 截取当前画面为 PNG base64（不含 `data:` 前缀）。失败抛错。 */
  captureScreenshot(): Promise<{ width: number; height: number; base64: string }>;
  /** 在截图坐标系点击（button：0=左 1=中 2=右）。失败抛错。 */
  click(x: number, y: number, button: number, double: boolean): void;
  /** 把文本输入到远端当前焦点（Unicode 通道）。失败抛错。 */
  typeText(text: string): void;
  /** 发送按键（code 为 DOM KeyboardEvent.code；modifiers 为其 code 数组）。失败抛错。 */
  keyPress(code: string, modifiers: string[]): void;
}

const controls = new Map<string, DesktopControl>();

/** RdpPane 挂载时注册控制句柄（键为桥接实例 id）。 */
export function registerDesktopControl(instanceId: string, control: DesktopControl) {
  controls.set(instanceId, control);
}

/** RdpPane 卸载时注销（会话关闭/标签关闭后 AI 再操作会得到明确错误）。 */
export function unregisterDesktopControl(instanceId: string) {
  controls.delete(instanceId);
}

/** 按桥接实例 id 取控制句柄；不存在（未打开/已关闭/VNC 会话）返回 undefined。 */
export function getDesktopControl(instanceId: string): DesktopControl | undefined {
  return controls.get(instanceId);
}

/** 桌面工具的前端执行结果（与后端 ToolResult 对应，可附截图图片）。 */
export interface DesktopToolExecResult {
  ok: boolean;
  output: string;
  /** desktop_screenshot 成功时附带（PNG）。 */
  image?: { mimeType: string; dataBase64: string };
}

/** desktop_type 单次输入上限（与后端工具描述一致，防失控注入长文本）。 */
const MAX_TYPE_CHARS = 2000;

/**
 * 在前端执行一个桌面工具。任何错误都被吞掉并返回 `ok=false` 的结果，
 * 由后端回填给模型让模型重试或向用户解释（与后端 `execute_tool` 的约定一致）。
 */
export async function executeDesktopTool(
  name: string,
  args: Record<string, unknown>,
  control: DesktopControl | undefined
): Promise<DesktopToolExecResult> {
  if (!control) {
    return {
      ok: false,
      output:
        "当前没有可用的内嵌 RDP 桌面会话：请先在桌面页打开 RDP 连接（内嵌模式）并保持会话在线",
    };
  }
  try {
    switch (name) {
      case "desktop_screenshot": {
        const shot = await control.captureScreenshot();
        return {
          ok: true,
          output: `已截取 RDP 桌面画面（${shot.width}x${shot.height}，PNG），截图以图片形式提供，请据此分析当前界面。`,
          image: { mimeType: "image/png", dataBase64: shot.base64 },
        };
      }
      case "desktop_click": {
        const { x, y } = args;
        if (
          typeof x !== "number" ||
          typeof y !== "number" ||
          !Number.isFinite(x) ||
          !Number.isFinite(y)
        ) {
          return { ok: false, output: "desktop_click 需要整数 x / y 参数" };
        }
        const btn = typeof args.button === "string" ? args.button : "left";
        const button = btn === "right" ? 2 : btn === "middle" ? 1 : 0;
        const double = args.double === true;
        const cx = Math.round(x);
        const cy = Math.round(y);
        control.click(cx, cy, button, double);
        const size = control.size();
        return {
          ok: true,
          output: `已在桌面 (${cx}, ${cy}) 完成${double ? "双击" : "点击"}（${btn} 键）。${
            size ? `当前桌面分辨率 ${size.width}x${size.height}。` : ""
          }建议调用 desktop_screenshot 确认效果。`,
        };
      }
      case "desktop_type": {
        const text = typeof args.text === "string" ? args.text : "";
        if (!text) return { ok: false, output: "desktop_type 需要 text 参数" };
        if (text.length > MAX_TYPE_CHARS) {
          return {
            ok: false,
            output: `文本过长（${text.length} 字符，上限 ${MAX_TYPE_CHARS}），请拆分后分段输入`,
          };
        }
        control.typeText(text);
        return { ok: true, output: `已输入 ${text.length} 个字符。` };
      }
      case "desktop_key": {
        const code = typeof args.code === "string" ? args.code : "";
        if (!code) {
          return {
            ok: false,
            output: "desktop_key 需要 code 参数（DOM KeyboardEvent.code，如 Enter / F5 / KeyA）",
          };
        }
        const mods = Array.isArray(args.modifiers)
          ? (args.modifiers as unknown[]).filter((m): m is string => typeof m === "string")
          : [];
        control.keyPress(code, mods);
        return { ok: true, output: `已发送按键 ${[...mods, code].join(" + ")}。` };
      }
      default:
        return { ok: false, output: `未知桌面工具: ${name}` };
    }
  } catch (e) {
    return {
      ok: false,
      output: `桌面操作失败: ${e instanceof Error ? e.message : String(e)}`,
    };
  }
}

/**
 * 把前端执行结果回传给后端编排循环（「批准/拒绝 + 执行结果」一体回执）。
 *
 * `approved=false` 表示用户拒绝（result 被忽略）；`approved=true` 时 result
 * 为执行结果，desktop_screenshot 的 result.image 会作为图片消息回填给模型。
 */
export function aiDesktopToolRespond(
  toolCallId: string,
  approved: boolean,
  result?: DesktopToolExecResult
): Promise<void> {
  return invoke<void>("ai_desktop_tool_respond", {
    toolCallId,
    approved,
    ok: result?.ok ?? false,
    output: result?.output ?? "用户拒绝了该操作",
    imageMime: result?.image?.mimeType ?? null,
    imageBase64: result?.image?.dataBase64 ?? null,
  });
}
