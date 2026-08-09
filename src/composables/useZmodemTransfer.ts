// lrzsz（ZMODEM）传输支持
// ----------------------------------------------------------------------------
// 基于 zmodem.js（FGasper fork）实现 rz 上传 / sz 下载：
// - 所有远端输出字节先喂给 Sentry 检测 ZMODEM 起始序列（ZRQINIT/ZRINIT），
//   非 ZMODEM 数据原样转发到终端（to_terminal），协议期间自动屏蔽回显。
// - 检测到会话后：远端 sz（receive 会话）→ 逐文件弹保存对话框落盘；
//   远端 rz（send 会话）→ 弹文件选择框逐文件流式发送。
// - sender 回调把协议字节写回 SSH channel，并串成 promise 链：
//   上传时等链上的写确认（terminal_write 的 ack 背压）再读下一块，
//   避免无界输入队列把整个文件堆进内存。
// ----------------------------------------------------------------------------

import { ref } from "vue";
import type { Terminal } from "@xterm/xterm";
import { open as openDialog, save as saveDialog } from "@tauri-apps/plugin-dialog";
import { create, open as openFile, stat } from "@tauri-apps/plugin-fs";
import { ElMessage } from "element-plus";
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
  sendRaw: (bytes: Uint8Array) => Promise<void>
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
    writeChain = writeChain
      .then(() => sendRaw(bytes))
      .catch(() => {
        /* 连接已断开：忽略，由 terminal:closed 事件兜底复位 */
      });
  }

  /** 等待已入队的发送全部完成（含 SSH 通道写确认）。 */
  function drainWrites(): Promise<void> {
    return writeChain;
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

  async function runReceive(sess: ZmodemSession) {
    sess.on("offer", (offer: ZmodemOffer) => {
      void receiveOffer(sess, offer);
    });
    // 发送 ZRINIT，远端收到后开始 ZFILE 报价。
    await sess.start();
  }

  async function receiveOffer(sess: ZmodemSession, offer: ZmodemOffer) {
    const details = offer.get_details();
    const name = (details.name || "unnamed").split(/[\\/]/).pop() || "unnamed";

    const path = await saveDialog({
      title: "保存文件（ZMODEM 下载）",
      defaultPath: name,
    });
    if (!path) {
      // 用户取消：跳过该文件，远端继续下一个或结束会话。
      offer.skip();
      return;
    }

    const handle = await create(path);
    progress.value = {
      kind: "download",
      name,
      transferred: 0,
      total: details.size ?? 0,
    };

    // 攒批落盘：input 回调只压缓冲，flush 链串行写文件（避免并发写交错）。
    let buffer: Uint8Array[] = [];
    let buffered = 0;
    let flushChain: Promise<void> = Promise.resolve();
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
      flushChain = flushChain.then(() => handle.write(all)).then(() => {});
    };

    try {
      await offer.accept({
        on_input: (octets) => {
          const chunk = new Uint8Array(octets);
          buffer.push(chunk);
          buffered += chunk.length;
          if (progress.value) progress.value.transferred += chunk.length;
          if (buffered >= FLUSH_THRESHOLD) flush();
        },
      });
      // ZEOF 后还有最后一批缓冲未刷盘。
      flush();
      await flushChain;
      if (!sess.aborted()) {
        ElMessage.success(`已下载：${name}`);
      }
    } catch (err) {
      if (!sess.aborted()) {
        console.warn("[zmodem] 下载失败", err);
        ElMessage.error(`下载失败：${err instanceof Error ? err.message : String(err)}`);
      }
    } finally {
      progress.value = null;
      await handle.close().catch(() => {});
    }
  }

  // ---------------------------------------------------------------------------
  // 上传：远端 rz ← 我们
  // ---------------------------------------------------------------------------

  async function runSend(sess: ZmodemSession) {
    const paths = await openDialog({
      title: "选择要上传的文件（ZMODEM）",
      multiple: true,
    });
    if (!paths || (Array.isArray(paths) && paths.length === 0)) {
      // 用户取消：发送中止序列让远端 rz 退出。
      sess.abort();
      return;
    }
    const list = Array.isArray(paths) ? paths : [paths];

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

        const xfer = await sess.send_offer({
          name,
          size: info.size ?? 0,
          mode: 0o644,
          mtime: info.mtime ? Math.floor(info.mtime.getTime() / 1000) : undefined,
        });
        if (!xfer) continue; // 远端跳过该文件

        const handle = await openFile(p, { read: true });
        try {
          const chunk = new Uint8Array(READ_CHUNK);
          let transferred = 0;
          for (;;) {
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
      }
    } catch (err) {
      if (!sess.aborted()) {
        console.warn("[zmodem] 上传失败", err);
        ElMessage.error(`上传失败：${err instanceof Error ? err.message : String(err)}`);
      }
      sess.abort();
    }
  }

  // ---------------------------------------------------------------------------
  // Sentry / 生命周期
  // ---------------------------------------------------------------------------

  function handleDetect(detection: ZmodemDetection) {
    // 防御：会话进行中再收到起始序列则拒绝（正常不会发生）。
    if (active.value) {
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
        }
        sess.abort();
      }
    })();
  }

  /** 初始化 Sentry（须在终端数据监听器注册之前调用）。 */
  function init() {
    sentry = new Zmodem.Sentry({
      to_terminal: (octets) => {
        getTerm()?.write(new Uint8Array(octets));
      },
      sender,
      on_detect: handleDetect,
      on_retract: () => {
        /* 误检测自动回退，无需处理 */
      },
    });
  }

  /** 喂入远端输出字节（终端数据监听器调用）。 */
  function feed(bytes: Uint8Array) {
    if (!sentry) return;
    try {
      sentry.consume(bytes);
    } catch (err) {
      // 协议异常（如对方中止）会抛错；session_end 事件负责复位状态。
      console.warn("[zmodem] consume 异常", err);
    }
  }

  /** 取消当前传输（发送 ZMODEM 中止序列，远端 rz/sz 会退出）。 */
  function cancel() {
    session?.abort();
  }

  /** 连接断开等场景的强制复位。 */
  function reset() {
    onSessionEnd();
  }

  return { active, progress, init, feed, cancel, reset };
}
