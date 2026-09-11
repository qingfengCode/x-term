/**
 * 终端输入行状态机（智能补全的基础设施）。
 *
 * 在浏览器侧维护一份"影子行缓冲"（用户当前输入行的字符数组 + 光标位置），
 * 并提供三类探测：
 * - 备用屏检测（vim/less/top 等全屏 TUI）：输出流出现 ESC[?1049h/l 时
 *   暂停一切补全逻辑（这些程序的输入不该被建议拦截）；
 * - 密码模式检测：本地缓冲在增长但终端光标 X 坐标不动 ⇒ 远端 echo 关闭，
 *   此时既不记历史也不出建议（防止密码进补全列表）；
 * - 当前命令提取（Enter 时）：从 xterm 屏幕缓冲自底向上扫提示符正则，
 *   拿到服务端回显的真实命令文本（含 tab 补全展开），比影子缓冲更可信。
 *
 * 借鉴自 uniTerm 的 useTerminalInput（Apache-2.0），适配 x-term 的组件结构：
 * 终端实例以 getter 注入（TerminalPane 的 term 是组件生命周期内的 let 变量）。
 */
import { computed, onBeforeUnmount, ref } from "vue";
import type { Terminal } from "@xterm/xterm";

export interface CursorPixelPos {
  x: number;
  y: number;
}

export interface UseTerminalInputOptions {
  /** Enter 时提取到完整命令的回调（记录历史用）。密码模式下不会触发。 */
  onHistoryExtract?: (command: string) => void;
  /** 新命令开始（Enter 后）时恢复补全的 Esc 抑制状态。 */
  onResetSuppress?: () => void;
  /** 是否记录历史（默认 true）。 */
  enableHistory?: boolean;
}

/** 提取命令的最大长度（超长多为粘贴脚本，不进历史/补全）。 */
const MAX_COMMAND_LENGTH = 200;

/** 提示符结尾字符：$ # > ] 及常见 zsh/oh-my-zsh/powerline 字形。 */
const PROMPT_RE = /(.+?[$#>\]❯➜→»λ])(?:\s+|$)(.*)/;

export function useTerminalInput(
  getTerminal: () => Terminal | null,
  options: UseTerminalInputOptions = {},
) {
  // 字符数组作为唯一事实来源：行中插入是 O(n) 的 splice，优于每次击键
  // 两次字符串拷贝 + 拼接。lineBuffer 是字符串镜像（模板/调用方用）。
  const lineChars = ref<string[]>([]);
  const lineBuffer = computed<string>({
    get: () => lineChars.value.join(""),
    set: (val) => {
      lineChars.value = Array.from(val);
    },
  });
  const cursorIndex = ref(0);
  /** 光标前的整行输入（补全匹配用的 token，如 "git che"）。 */
  const currentToken = ref("");
  /** 光标像素位置（相对 xterm 元素原点，建议弹窗定位用）。 */
  const cursorPixelPos = ref<CursorPixelPos>({ x: 0, y: 0 });

  let inAlternateScreen = false;
  let isPasswordPrompt = false;
  let cursorPosRAF: number | null = null;
  let lastTerminalCursorX = -1;
  // 密码模式确认计时器：击键后留出回显 RTT 窗口再下结论（见 updateCursorPosition）。
  let passwordConfirmTimer: ReturnType<typeof setTimeout> | null = null;
  // 备用屏检测的跨界残留：SSH 输出批次边界任意，序列可能被拆在两批之间，
  // 保留上一批尾部与下一批拼接后再检测（见 handleSessionData）。
  let altScreenCarry = "";

  function stripAnsi(str: string): string {
    return str
      // OSC 序列：ESC ] ... BEL 或 ESC ] ... ESC \
      .replace(/\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)/g, "")
      // CSI 序列：ESC [ params final-byte
      .replace(/\x1b\[[0-?]*[ -/]*[@-~]/g, "")
      // 单字符 FE 转义：ESC @ 到 ESC _、ESC ` 到 ESC ~
      .replace(/\x1b[@-Z\-_]/g, "")
      // 字符集指定：ESC ( B、ESC ) B 等
      .replace(/\x1b[()[\]{}][0-9A-Za-z]/g, "");
  }

  /** 从 xterm 屏幕缓冲提取当前命令（信屏幕不信影子缓冲）。
   *
   * 自底向上扫可见区找最新提示符行；长命令折行时沿 isWrapped 续行向下拼接。
   * 返回 null 表示屏幕上无可识别的命令行。 */
  function getCurrentCommandFromTerminal(): string | null {
    const terminal = getTerminal();
    if (!terminal) return null;
    try {
      const buffer = terminal.buffer.active;
      const rows = terminal.rows || 24;
      const bottomY = buffer.baseY + rows - 1;
      for (let dy = 0; dy < rows; dy++) {
        const y = bottomY - dy;
        if (y < 0) break;
        const line = buffer.getLine(y);
        if (!line) continue;
        const rawText = line.translateToString(true).trim();
        if (!rawText) continue;
        const cleanText = stripAnsi(rawText);
        const match = cleanText.match(PROMPT_RE);
        if (!match) continue;
        const command = match[2].trim();
        // AI 可视化执行的哨兵包装命令不进历史。
        if (!command || command.includes("__XTERM_DONE_")) continue;
        let full = command;
        for (let cy = y + 1; cy <= bottomY; cy++) {
          const cont = buffer.getLine(cy);
          // xterm v5 的 isWrapped 是属性（v6 才是方法）。
          if (!cont || !cont.isWrapped) break;
          full += stripAnsi(cont.translateToString(true).trim());
        }
        if (full.length > MAX_COMMAND_LENGTH) continue;
        return full;
      }
    } catch {
      // buffer 访问失败（终端已销毁等）忽略
    }
    return null;
  }

  /** 用光标前的整行（而非最后一个词）匹配建议：输入 "git sta" 能命中
   * 历史里的 "git status --short"。 */
  function updateToken() {
    const buf = lineChars.value;
    const idx = cursorIndex.value;
    let beforeCursor = "";
    for (let i = 0; i < idx && i < buf.length; i++) beforeCursor += buf[i];
    currentToken.value = beforeCursor.trim();
  }

  /** 更新光标像素位置（相对 xterm 元素原点）+ 密码模式探测。 */
  function updateCursorPosition() {
    const terminal = getTerminal();
    if (!terminal) {
      cursorPixelPos.value = { x: 0, y: 0 };
      return;
    }
    try {
      const buffer = terminal.buffer.active;
      const cursorX = buffer.cursorX;
      // 单元格像素尺寸：xterm v5 渲染器内部维度（css 像素）。
      const core = (terminal as unknown as { _core?: unknown })._core as
        | {
            _renderService?: {
              dimensions?: { css?: { cell?: { width?: number; height?: number } } };
            };
          }
        | undefined;
      const cell = core?._renderService?.dimensions?.css?.cell;
      const cellWidth = cell?.width || 9;
      const cellHeight = cell?.height || 17;
      cursorPixelPos.value = {
        x: cursorX * cellWidth,
        y: (buffer.cursorY + 1) * cellHeight,
      };
      // 密码模式探测：本地缓冲在增长但终端光标 X 不动 ⇒ 远端 echo 关闭。
      // 不能在击键后的下一帧立即下结论——远端回显需要 RTT（数十 ms），此时
      // 光标往往还没动，快速连打会把正常命令误判成密码（漏记历史、误关
      // 建议）。挂 150ms 确认窗：期间光标动了（回显到达）即排除；光标移动
      // 一律立即排除。Enter 时刻另有同步兜底（见 handleInput）。
      if (lineChars.value.length > 0 && cursorX === lastTerminalCursorX && cursorX >= 0) {
        if (passwordConfirmTimer) clearTimeout(passwordConfirmTimer);
        passwordConfirmTimer = setTimeout(() => {
          passwordConfirmTimer = null;
          try {
            const x = getTerminal()?.buffer.active.cursorX;
            if (lineChars.value.length > 0 && x === lastTerminalCursorX && x !== undefined) {
              isPasswordPrompt = true;
            }
          } catch {
            // 终端已销毁等，忽略
          }
        }, 150);
      } else if (cursorX !== lastTerminalCursorX) {
        if (passwordConfirmTimer) {
          clearTimeout(passwordConfirmTimer);
          passwordConfirmTimer = null;
        }
        isPasswordPrompt = false;
      }
      lastTerminalCursorX = cursorX;
    } catch {
      cursorPixelPos.value = { x: 0, y: 0 };
    }
  }

  /** 当前终端光标 X 是否仍与上次采样一致（回显未到达的判据，Enter 兜底用）。 */
  function terminalCursorUnmoved(): boolean {
    try {
      const x = getTerminal()?.buffer.active.cursorX;
      return x !== undefined && x >= 0 && x === lastTerminalCursorX;
    } catch {
      return false;
    }
  }

  function isAtLineEnd(): boolean {
    return cursorIndex.value >= lineChars.value.length;
  }

  /** 喂入一段用户输入（term.onData 的 data）。解析可打印字符与行编辑键，
   * 维护影子缓冲；Enter 时提取命令记录历史并清行。 */
  function handleInput(data: string) {
    if (inAlternateScreen) return;
    for (let i = 0; i < data.length; i++) {
      const char = data[i];
      const code = data.charCodeAt(i);
      if (char === "\r" || char === "\n") {
        // Enter：融合影子缓冲与屏幕提取。
        // - 屏幕版以影子为前缀 ⇒ 同一行且屏幕更完整（Tab 补全展开/折行拼接），
        //   用屏幕版；
        // - 不匹配 ⇒ 当前行回显尚未到达（高延迟链路），屏幕上扫到的是**上一条**
        //   已执行命令的行——必须用影子缓冲，否则会错记上一条；
        // - 影子为空（纯屏幕场景，如粘贴后直接回车）用屏幕版兜底。
        // Enter 时刻同步兜底：150ms 确认窗未到点时，若屏幕光标相对上次采样
        // 仍未移动（echo 关闭），按密码处理——宁可漏记一条历史，也不能把
        // 密码写进历史库。
        const passwordLike =
          isPasswordPrompt || (lineChars.value.length > 0 && terminalCursorUnmoved());
        if (passwordConfirmTimer) {
          clearTimeout(passwordConfirmTimer);
          passwordConfirmTimer = null;
        }
        if (options.enableHistory !== false && !passwordLike) {
          const shadow = lineBuffer.value.trim();
          const screen = getCurrentCommandFromTerminal();
          let command: string;
          if (shadow && screen) {
            command = screen.startsWith(shadow) ? screen : shadow;
          } else {
            command = shadow || screen || "";
          }
          if (command && options.onHistoryExtract) {
            options.onHistoryExtract(command);
          }
        }
        if (isPasswordPrompt) isPasswordPrompt = false;
        lineChars.value = [];
        cursorIndex.value = 0;
        if (options.onResetSuppress) options.onResetSuppress();
      } else if (code === 127 || char === "\b") {
        // Backspace
        if (cursorIndex.value > 0) {
          lineChars.value.splice(cursorIndex.value - 1, 1);
          cursorIndex.value--;
        }
      } else if (code === 3) {
        // Ctrl+C：shell 抛弃当前输入行直接给新提示符——影子缓冲必须同步
        // 清空，否则后续补全 token 残留已作废的输入。
        lineChars.value = [];
        cursorIndex.value = 0;
      } else if (code === 1) {
        // Ctrl+A — 行首
        cursorIndex.value = 0;
      } else if (code === 5) {
        // Ctrl+E — 行尾
        cursorIndex.value = lineChars.value.length;
      } else if (code === 11) {
        // Ctrl+K — 删到行尾
        lineChars.value.length = cursorIndex.value;
      } else if (code === 21) {
        // Ctrl+U — 删到行首
        lineChars.value.splice(0, cursorIndex.value);
        cursorIndex.value = 0;
      } else if (code === 27) {
        // CSI / SS3 序列（方向键 / Home / End / Delete / 功能键）
        i++;
        if (data[i] === "O") {
          // SS3（ESC O <最终字节>）：DECCKM 应用光标模式下 xterm 对方向键
          // （A/B/C/D）及 F1-F4（P~S）的编码。多消费一个最终字节，防止它
          // 落入下方可打印分支把字母拼进影子行缓冲（污染补全 token/历史）。
          i++;
        } else if (data[i] === "[") {
          i++;
          let param = "";
          while (i < data.length && ((data[i] >= "0" && data[i] <= "9") || data[i] === ";")) {
            param += data[i];
            i++;
          }
          const finalChar = data[i];
          if (finalChar === "D") {
            if (cursorIndex.value > 0) cursorIndex.value--;
          } else if (finalChar === "C") {
            if (cursorIndex.value < lineChars.value.length) cursorIndex.value++;
          } else if (finalChar === "H" && param === "") {
            cursorIndex.value = 0;
          } else if (finalChar === "F" && param === "") {
            cursorIndex.value = lineChars.value.length;
          } else if (finalChar === "~") {
            if (param === "1" || param === "7") {
              cursorIndex.value = 0;
            } else if (param === "4" || param === "8") {
              cursorIndex.value = lineChars.value.length;
            } else if (param === "3") {
              if (cursorIndex.value < lineChars.value.length) {
                lineChars.value.splice(cursorIndex.value, 1);
              }
            }
          }
        }
      } else if (code >= 32) {
        // 可打印字符（含 CJK）入缓冲
        lineChars.value.splice(cursorIndex.value, 0, char);
        cursorIndex.value++;
      }
    }
    updateToken();
    // 光标位置更新合并到下一绘制帧（rAF）：连续击键时一帧只算一次。
    if (cursorPosRAF !== null) cancelAnimationFrame(cursorPosRAF);
    cursorPosRAF = requestAnimationFrame(() => {
      cursorPosRAF = null;
      updateCursorPosition();
    });
  }

  /** 备用屏进出标记 → 目标状态。最长 8 字节（"\x1b[?1049h"）。 */
  const ALT_SCREEN_MARKERS: Array<[string, boolean]> = [
    ["\x1b[?1049h", true],
    ["\x1b[?47h", true],
    ["\x1b[?1049l", false],
    ["\x1b[?47l", false],
  ];

  /** 喂入一段会话输出字节，检测备用屏进入/退出。
   *
   * 字节级预扫：不含 ESC 的批次（绝大多数普通输出）直接跳过，不做字符串
   * 转换。转义序列全是 ASCII，Latin-1 解码字节为字符不会产生误匹配；多字节
   * UTF-8 字符被拆成的孤立字节里出现完整 `ESC[?1049h` 序列的概率可忽略。
   *
   * 批次边界任意（PTY 按块读），退出序列可能被拆在两批之间——单批检测
   * 会永久丢失状态翻转（补全从此不再弹出/在 vim 内误弹）。保留上一批
   * 尾部与下一批拼接后检测，按**最后出现**的标记生效。 */
  function handleSessionData(bytes: Uint8Array) {
    // 快速路径：无 ESC 且无跨界残留的批次不可能包含/补全序列。
    if (bytes.indexOf(0x1b) === -1 && altScreenCarry === "") return;
    const text = altScreenCarry + bytesToLatin1(bytes);
    let bestPos = -1;
    let bestVal: boolean | null = null;
    for (const [marker, val] of ALT_SCREEN_MARKERS) {
      const pos = text.lastIndexOf(marker);
      if (pos > bestPos) {
        bestPos = pos;
        bestVal = val;
      }
    }
    if (bestVal !== null) inAlternateScreen = bestVal;
    // 保留可能被截断的尾部（覆盖最长标记 8 字节），与下一批拼接。
    altScreenCarry = text.length > 16 ? text.slice(-16) : text;
  }

  function clearBuffer() {
    lineChars.value = [];
    cursorIndex.value = 0;
    currentToken.value = "";
  }

  function isInAlternateScreen(): boolean {
    return inAlternateScreen;
  }

  function isPasswordMode(): boolean {
    return isPasswordPrompt;
  }

  // 宿主组件卸载时清理待决的定时器/动画帧：销毁瞬间回调仍会触发，里面
  // 访问已 dispose 的 xterm 缓冲虽被 try/catch 兜住，但没必要留隐患。
  onBeforeUnmount(() => {
    if (passwordConfirmTimer) {
      clearTimeout(passwordConfirmTimer);
      passwordConfirmTimer = null;
    }
    if (cursorPosRAF !== null) {
      cancelAnimationFrame(cursorPosRAF);
      cursorPosRAF = null;
    }
  });

  return {
    lineBuffer,
    cursorIndex,
    currentToken,
    cursorPixelPos,
    isAtLineEnd,
    handleInput,
    handleSessionData,
    clearBuffer,
    isInAlternateScreen,
    isPasswordMode,
  };
}

/** 字节流 → Latin-1 文本（备用屏检测用，转义序列均为 ASCII）。 */
export function bytesToLatin1(bytes: Uint8Array): string {
  let out = "";
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    out += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return out;
}

/** 需要向外暴露的类型。 */
export type TerminalInputState = ReturnType<typeof useTerminalInput>;
