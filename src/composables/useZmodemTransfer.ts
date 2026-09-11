// lrzsz（ZMODEM）传输支持
// ----------------------------------------------------------------------------
// 基于 zmodem.js（FGasper fork）实现 rz 上传 / sz 下载：
// - 所有远端输出字节先喂给 Sentry 检测 ZMODEM 起始序列（ZRQINIT/ZRINIT），
//   非 ZMODEM 数据原样转发到终端（to_terminal），协议期间自动屏蔽回显。
// - 检测到会话后：远端 sz（receive 会话）→ 每个文件解析保存路径后落盘
//   （设置了默认下载目录时直接落盘并自动重命名，否则逐文件弹保存对话框），
//   并在全局下载列表（transfer store，框架顶部可呼出）记录进度；
//   远端 rz（send 会话）→ 弹文件选择框逐文件流式发送。
//   对话框走无父窗口的自定义命令（api/terminal.ts zmodemPickFiles/
//   zmodemSaveFile）：带 owner 的原生模态框会禁用主窗口，期间 WebView2
//   会把光标置为隐藏（弹窗内鼠标不可见），无 owner 则不触发。
// - sender 回调把协议字节写回 SSH channel，并串成 promise 链：
//   上传时等链上的写确认（terminal_write 的 ack 背压）再读下一块，
//   避免无界输入队列把整个文件堆进内存。
// ----------------------------------------------------------------------------

import { ref } from "vue";
import type { Terminal } from "@xterm/xterm";
import { create, open as openFile, stat } from "@tauri-apps/plugin-fs";
import { ElMessage } from "element-plus";
import { zmodemPickFiles, zmodemSaveFile } from "@/api/terminal";
import { configuredDownloadDir, uniquePathIn, usableDownloadDir } from "@/utils/downloadPath";
import { useTransferStore } from "@/stores/transfer";
import Zmodem from "zmodem.js";
import type {
  Detection as ZmodemDetection,
  Offer as ZmodemOffer,
  Session as ZmodemSession,
  Transfer as ZmodemTransfer,
} from "zmodem.js";

/** 传输进度（进度浮层展示用）。 */
export interface ZmodemProgress {
  kind: "upload" | "download";
  name: string;
  transferred: number;
  total: number;
}

/** 下载落盘缓冲上限：攒够该字节数再一次性写盘，减少 IPC 次数。 */
const FLUSH_THRESHOLD = 512 * 1024;

/** 上传每次从本地文件读入的块大小（内部会再切成 8KiB 的 ZMODEM 子包）。 */
const READ_CHUNK = 1024 * 1024;

export function useZmodemTransfer(
  getTerm: () => Terminal | null,
  sendRaw: (bytes: Uint8Array) => Promise<void>,
  /**
   * 输出写入钩子（经 Sentry 放行的非协议字节 → 终端渲染）。
   * 默认原样写入；TerminalPane 注入多字符编码（GBK 等）的流式解码。
   */
  writeOutput: (bytes: Uint8Array) => void = (bytes) => {
    getTerm()?.write(bytes);
  },
) {
  /** 是否有进行中的 ZMODEM 会话（用于门控键盘输入）。 */
  const active = ref(false);
  /** 当前传输进度；null 表示尚未开始传输数据。 */
  const progress = ref<ZmodemProgress | null>(null);

  let sentry: InstanceType<typeof Zmodem.Sentry> | null = null;
  let session: ZmodemSession | null = null;

  // 发送链：协议字节串行发送并等待写确认，提供背压。
  let writeChain: Promise<void> = Promise.resolve();

  function sender(octets: number[]) {
    const bytes = new Uint8Array(octets);
    writeChain = writeChain.then(() => sendRaw(bytes)).catch(() => {
      // 连接已断开：中止当前会话，让传输流程尽快退出
      // （否则下载的 accept 永久挂起 / 上传把整个文件读完后无意义重发）。
      if (session && !session.aborted()) session.abort();
    });
  }

  /** 等待已入队的发送全部完成（含 SSH 通道写确认）。 */
  function drainWrites(): Promise<void> {
    return writeChain;
  }

  /**
   * 补发强化的 ZMODEM 中止序列（16×CAN + 8×BS）。
   *
   * zmodem.js 的 abort() 只发 5×CAN+5×BS：远端 rz 若正在等 ZFILE 的读取
   * 中途错过前几个 CAN（短突发跨越其读取边界），不会识别为取消，就一直
   * 占着 tty 等文件——表现为终端停在 rz 界面回不到 shell 提示符。更长的
   * 连续 CAN 突发保证远端在任意解析状态下都能识别取消。走 sender() 排入
   * 同一发送链，保证与协议字节的先后顺序。
   */
  function sendHardAbort() {
    const octets: number[] = [];
    for (let i = 0; i < 16; i++) octets.push(0x18); // CAN
    for (let i = 0; i < 8; i++) octets.push(0x08); // BS（擦除 CAN 残留）
    sender(octets);
  }

  /** 取消后的检测抑制窗口：远端残留的 ZMODEM 起始帧（ZRQINIT 重试）在
   *  窗口内不触发新检测，避免"取消后弹框再次弹出"。 */
  let suppressUntil = 0;
  function suppressDetection(ms = 2000) {
    suppressUntil = Date.now() + ms;
  }

  // --- 取消后的静默排空 -------------------------------------------------------
  // 本地 abort() 后 Sentry 立即回到透传模式，而远端 sz/rz 要等收到 CAN
  // 序列并排空网络在途数据后才真正停发——这个窗口内倾泻进来的协议字节
  // （ZDATA 帧、退出信息）会被透传进终端刷成乱码。取消后进入排空模式：
  // 只吞不显，数据流出现静默间隙（远端停发）或超过最长窗口后恢复透传。
  let discarding = false;
  let discardDeadline = 0;
  let discardGapTimer: ReturnType<typeof setTimeout> | null = null;

  /** 进入静默排空模式（取消传输时调用）。 */
  function startDiscard(maxMs = 3000) {
    discarding = true;
    discardDeadline = Date.now() + maxMs;
    armDiscardGap();
  }

  /** 每收到一段数据重新计时；间隔超时视为远端已停发，恢复透传。 */
  function armDiscardGap() {
    if (discardGapTimer) clearTimeout(discardGapTimer);
    discardGapTimer = setTimeout(() => {
      discarding = false;
      discardGapTimer = null;
    }, 200);
  }

  /** 排空模式下的吞吐判定：true 表示该段数据应被丢弃。 */
  function shouldDiscard(): boolean {
    if (!discarding) return false;
    if (Date.now() > discardDeadline) {
      // 超过最长排空窗口：强制恢复透传（避免吞掉后续正常输出/提示符）。
      if (discardGapTimer) clearTimeout(discardGapTimer);
      discardGapTimer = null;
      discarding = false;
      return false;
    }
    armDiscardGap();
    return true;
  }

  /** 给会话收尾类调用加超时保护：远端无响应时避免 promise 永远挂起。 */
  function withTimeout<T>(promise: Promise<T>, ms: number): Promise<T | undefined> {
    return Promise.race([
      promise.then((v) => v as T),
      new Promise<undefined>((r) => setTimeout(() => r(undefined), ms)),
    ]);
  }

  /** 会话结束（正常结束 / 取消 / 对方中止）统一回收状态。 */
  function onSessionEnd() {
    active.value = false;
    progress.value = null;
    session = null;
  }

  // ---------------------------------------------------------------------------
  // 下载：远端 sz → 我们
  // ---------------------------------------------------------------------------

  /** 最近一次收到 offer 的时刻：sz 取消保存框后判定对端是否已卡死。 */
  let lastOfferAt = 0;

  // store 在传输回调运行时获取（pinia 此时已激活；避免模块顶层依赖）。
  const transfer = useTransferStore();

  /**
   * 解析保存路径：设置了默认下载目录且目录仍存在 → 直接在目录内自动命名
   * （同名文件加 " (n)" 后缀，不弹框）；否则回退保存对话框（用户取消返回
   * null）。默认目录失效（被删除/移动）时提示一次并回退对话框，不静默失败。
   */
  async function resolveSavePath(name: string): Promise<string | null> {
    const dir = await usableDownloadDir();
    if (dir) return uniquePathIn(dir, name);
    const configured = configuredDownloadDir();
    if (configured) {
      ElMessage.warning(`默认下载目录不存在，已改用另存为对话框：${configured}`);
    }
    return zmodemSaveFile("保存文件（ZMODEM 下载）", name).catch(() => null);
  }

  async function runReceive(sess: ZmodemSession) {
    sess.on("offer", (offer: ZmodemOffer) => {
      lastOfferAt = Date.now();
      void receiveOffer(sess, offer);
    });
    // 发送 ZRINIT，远端收到后开始 ZFILE 报价。
    await sess.start();
  }

  async function receiveOffer(sess: ZmodemSession, offer: ZmodemOffer) {
    const details = offer.get_details();
    const name = (details.name || "unnamed").split(/[\\/]/).pop() || "unnamed";

    // 对话框命令异常（后端错误）按取消处理，避免走"传输异常"误报路径。
    const path = await resolveSavePath(name).catch(() => null);
    if (!path) {
      // 用户取消：跳过该文件，远端继续下一个或结束会话。对端若就此发
      // ZFIN 结束，同样需要我方回 ZFIN 握手，否则 sz 挂起不退回提示符。
      offer.skip();
      try {
        await withTimeout(sess.close(), 15_000);
      } catch {
        /* 还有后续 offer 时 close 会被拒绝/挂起到超时，均忽略 */
      }
      // close 超时且会话仍存活、等待期间也没有新 offer 到来：对端已卡死，
      // 强制中止（强化 CAN + 静默排空）——否则键盘被门控、终端回不到提示符。
      if (!sess.aborted() && Date.now() - lastOfferAt > 1500) {
        startDiscard();
        sendHardAbort();
        sess.abort();
      }
      return;
    }

    // 创建本地文件失败（目录只读/文件名含非法字符等）时必须中止会话：
    // offer 未 accept/skip 的话远端 sz 挂起、不会产生 session_end，
    // active 永久为 true，终端键盘被门控（只能断开重连）。
    let handle: Awaited<ReturnType<typeof create>>;
    try {
      handle = await create(path);
    } catch (e) {
      ElMessage.error(`下载失败：无法创建文件 ${path}（${String(e)}）`);
      startDiscard();
      sendHardAbort();
      sess.abort();
      return;
    }
    progress.value = {
      kind: "download",
      name,
      transferred: 0,
      total: details.size ?? 0,
    };

    // 下载列表记录（框架顶部呼出）：进度随 on_input 更新，结束按结果落状态。
    const taskId = `zmodem-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;
    transfer.add({
      id: taskId,
      name,
      direction: "download",
      transferred: 0,
      total: details.size ?? 0,
      status: "running",
      source: "zmodem",
      localPath: path,
    });

    // 攒批落盘：input 回调只压缓冲，flush 链串行写文件（避免并发写交错）。
    // 注意 flush 链不吞错：写盘失败让 rejection 传播到 await flushChain，
    // 由 catch 提示"下载失败"，避免磁盘满/权限错误时误报"已下载"。
    let buffer: Uint8Array[] = [];
    let buffered = 0;
    let flushChain: Promise<unknown> = Promise.resolve();
    const flush = () => {
      if (!buffer.length) return;
      const all = new Uint8Array(buffered);
      let off = 0;
      for (const chunk of buffer) {
        all.set(chunk, off);
        off += chunk.length;
      }
      buffer = [];
      buffered = 0;
      flushChain = flushChain.then(() => handle.write(all));
    };

    // 会话结束（连接断开/取消/对端中止）时立即结束 accept：zmodem.js 的
    // accept promise 只在收到 ZEOF 时才 settle，不兜底会永久挂起、句柄不关。
    // sessionEndedFired 标记用于完成路径判定 close() 后会话是否真的结束。
    let sessionEndedFired = false;
    const sessionEnded = new Promise<void>((resolve) => {
      sess.on("session_end", () => {
        sessionEndedFired = true;
        resolve();
      });
    });

    // 60 秒无数据视为远端挂死，主动中止（正常传输中 on_input 持续刷新计时）。
    let timedOut = false;
    let inactivityTimer: ReturnType<typeof setTimeout> | null = null;
    const refreshTimer = () => {
      if (inactivityTimer) clearTimeout(inactivityTimer);
      inactivityTimer = setTimeout(() => {
        timedOut = true;
        if (!sess.aborted()) sess.abort();
      }, 60_000);
    };

    try {
      await Promise.race([
        offer.accept({
          on_input: (octets) => {
            refreshTimer();
            const chunk = new Uint8Array(octets);
            buffer.push(chunk);
            buffered += chunk.length;
            if (progress.value) progress.value.transferred += chunk.length;
            transfer.update(taskId, { transferred: progress.value?.transferred ?? 0 });
            if (buffered >= FLUSH_THRESHOLD) flush();
          },
        }),
        sessionEnded,
      ]);
      // ZEOF 后还有最后一批缓冲未刷盘。
      flush();
      await flushChain;
      if (timedOut) {
        transfer.update(taskId, {
          status: "error",
          message: "远端 60 秒无数据，已中止",
          endedAt: Date.now(),
        });
        ElMessage.error(`下载失败：${name}（远端 60 秒无数据，已中止）`);
      } else if (sess.aborted()) {
        transfer.update(taskId, { status: "cancelled", endedAt: Date.now() });
        ElMessage.warning(`下载已中止：${name}`);
      } else {
        transfer.update(taskId, { status: "done", endedAt: Date.now() });
        ElMessage.success(`已下载：${name}`);
      }
      // ZFIN 收尾握手（与上传路径的 sess.close() 对称）：ZEOF 只表示数据发完，
      // 远端 sz 要收到我方 ZFIN 才会退出。缺这一步 sz 永久等待，终端停在
      // "**B00000000000000"（sz 的 ZFIN 帧文本）不回 shell 提示符。
      // 对端无响应时 15s 超时兜底，避免挂起。
      if (!sess.aborted()) {
        try {
          await withTimeout(sess.close(), 15_000);
        } catch {
          /* 已结束/重复关闭等，忽略 */
        }
        // close 超时/对端未回 ZFIN：session_end 不触发会让 active 卡在
        // true（键盘被永久门控），远端 sz 也不退出。强制中止收尾——
        // abort 自带 5×CAN+5×BS 并触发 session_end 复位状态；强化 CAN
        // 序列确保远端退出。
        if (!sessionEndedFired && !sess.aborted()) {
          sendHardAbort();
          sess.abort();
        }
      }
    } catch (err) {
      if (!sess.aborted()) {
        console.warn("[zmodem] 下载失败", err);
        transfer.update(taskId, {
          status: "error",
          message: err instanceof Error ? err.message : String(err),
          endedAt: Date.now(),
        });
        ElMessage.error(`下载失败：${err instanceof Error ? err.message : String(err)}`);
      } else {
        transfer.update(taskId, { status: "cancelled", endedAt: Date.now() });
      }
    } finally {
      if (inactivityTimer) clearTimeout(inactivityTimer);
      progress.value = null;
      await handle.close().catch(() => {});
    }
  }

  // ---------------------------------------------------------------------------
  // 上传：远端 rz ← 我们
  // ---------------------------------------------------------------------------

  async function runSend(sess: ZmodemSession) {
    // 对话框命令异常（后端错误）按取消处理，避免走"传输异常"误报路径。
    const paths = await zmodemPickFiles("选择要上传的文件（ZMODEM）").catch(() => [] as string[]);
    if (!paths || paths.length === 0) {
      // 用户取消：中止会话（发送 5×CAN+5×BS）并补发强化中止序列，确保
      // 远端 rz 退出、终端回到 shell 提示符；进入检测抑制窗口防止远端残留
      // 的 ZRQINIT 重试帧再次弹框；静默排空吞掉取消瞬间的在途字节。
      suppressDetection();
      startDiscard();
      // reset()（如连接断开）可能已中止过会话：二次 abort 会抛 already_aborted。
      if (!sess.aborted()) sess.abort();
      sendHardAbort();
      return;
    }
    const list = Array.isArray(paths) ? paths : [paths];

    // 会话结束兜底：连接断开后 zmodem.js 的 send_offer promise 可能永不
    // settle，race 保证 runSend 能退出（退出后由 aborted 守卫逐层 return）。
    // sendEndedFired 标记用于完成路径判定 close() 后会话是否真的结束。
    let sendEndedFired = false;
    const sendEnded = new Promise<null>((resolve) => {
      sess.on("session_end", () => {
        sendEndedFired = true;
        resolve(null);
      });
    });

    try {
      for (const p of list) {
        if (sess.aborted()) return;
        const name = p.split(/[\\/]/).pop() || "file";
        const info = await stat(p);
        progress.value = {
          kind: "upload",
          name,
          transferred: 0,
          total: info.size ?? 0,
        };

        const xfer = await Promise.race([
          sess.send_offer({
            name,
            size: info.size ?? 0,
            mode: 0o644,
            mtime: info.mtime ? Math.floor(info.mtime.getTime() / 1000) : undefined,
          }),
          sendEnded,
        ]);
        if (!xfer) continue; // 远端跳过该文件 / 会话已结束

        const handle = await openFile(p, { read: true });
        try {
          const chunk = new Uint8Array(READ_CHUNK);
          let transferred = 0;
          for (;;) {
            // 连接断开（sender 写失败已 abort）：立即停止读盘，避免把整个
            // 文件无意义地读完（drainWrites 因 sender 吞错会立即返回）。
            if (sess.aborted()) break;
            const n = await handle.read(chunk);
            if (n === null || n === 0) break;
            const slice = n === chunk.length ? chunk : chunk.slice(0, n);
            xfer.send(slice);
            transferred += n;
            if (progress.value) progress.value.transferred = transferred;
            // 背压：等这一块全部写入 SSH 通道（含确认）再读下一块。
            await drainWrites();
          }
          await xfer.end();
        } finally {
          await handle.close().catch(() => {});
        }
      }
      if (!sess.aborted()) {
        // ZFIN 收尾握手，远端无响应时 15s 超时兜底，避免挂起。
        await withTimeout(sess.close(), 15_000);
        ElMessage.success("上传完成");
        // close 超时/对端未回 ZFIN：与下载路径对称的强制收尾，防止 active
        // 卡 true（键盘被门控）、远端 rz 不退出。
        if (!sendEndedFired && !sess.aborted()) {
          sendHardAbort();
          sess.abort();
        }
      }
    } catch (err) {
      if (!sess.aborted()) {
        console.warn("[zmodem] 上传失败", err);
        ElMessage.error(`上传失败：${err instanceof Error ? err.message : String(err)}`);
        // abort() 二次调用会抛 already_aborted，先判 aborted 状态。
        sess.abort();
      }
    }
  }

  // ---------------------------------------------------------------------------
  // Sentry / 生命周期
  // ---------------------------------------------------------------------------

  function handleDetect(detection: ZmodemDetection) {
    // 防御：会话进行中，或刚取消过传输（远端残留起始帧重试）时拒绝新检测。
    if (active.value || Date.now() < suppressUntil) {
      detection.deny();
      return;
    }
    const sess = detection.confirm();
    session = sess;
    active.value = true;
    sess.on("session_end", onSessionEnd);
    void (async () => {
      try {
        if (sess.type === "receive") {
          await runReceive(sess);
        } else {
          await runSend(sess);
        }
      } catch (err) {
        console.warn("[zmodem] 会话异常", err);
        if (!sess.aborted()) {
          ElMessage.error(`传输异常：${err instanceof Error ? err.message : String(err)}`);
          sess.abort();
        }
      }
    })();
  }

  /** 初始化 Sentry（须在终端数据监听器注册之前调用）。 */
  function init() {
    sentry = new Zmodem.Sentry({
      to_terminal: (octets) => {
        writeOutput(new Uint8Array(octets));
      },
      sender,
      on_detect: handleDetect,
      on_retract: () => {
        /* 误检测自动回退，无需处理 */
      },
    });
  }

  /** 喂入远端输出字节（终端数据监听器调用）。
   *  取消后的静默排空窗口内直接丢弃（否则远端残留协议字节刷成乱码）。 */
  function feed(bytes: Uint8Array) {
    if (!sentry) return;
    if (shouldDiscard()) return;
    try {
      sentry.consume(bytes);
    } catch (err) {
      // 协议异常（如对方中止）会抛错；session_end 事件负责复位状态。
      console.warn("[zmodem] consume 异常", err);
    }
  }

  /** 取消当前传输（进度浮层的"取消"按钮）：中止会话 + 补发强化中止序列，
   *  并进入静默排空——远端 sz/rz 收到取消到真正停发之间的在途协议字节
   *  直接丢弃，不透传成乱码。 */
  function cancel() {
    if (session && !session.aborted()) {
      startDiscard();
      session.abort();
      sendHardAbort();
    }
  }

  /** 连接断开/重绑等场景的强制复位：先中止会话让 zmodem.js 状态机退出，
   *  再清引用。同时终止静默排空窗口——否则窗口内新会话的 attach 快照与
   *  首批 live 事件会被 shouldDiscard 整段吞掉，新会话首屏空白。 */
  function reset() {
    if (session && !session.aborted()) session.abort();
    if (discardGapTimer) clearTimeout(discardGapTimer);
    discardGapTimer = null;
    discarding = false;
    onSessionEnd();
  }

  return { active, progress, init, feed, cancel, reset };
}
