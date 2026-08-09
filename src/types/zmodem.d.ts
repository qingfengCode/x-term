/**
 * zmodem.js（FGasper fork，npm `zmodem.js@0.1.10`）的 TypeScript 类型声明。
 *
 * 该包是 CJS、无内置类型，且 `index.js` 通过 `Object.assign(module.exports, ...)`
 * 只暴露默认导出（一个带静态成员的对象）。这里仅声明本项目用到的 API 面；
 * `Offer` / `Transfer` / `Detection` 是协议回调的入参类型（运行时仅出现在
 * 回调中，不作为模块导出成员），故只做类型导出，构建时会被擦除。
 */

declare module "zmodem.js" {
  /** 文件信息（上传报价参数 / 下载 offer 详情共用）。 */
  export interface FileDetails {
    name: string;
    size?: number;
    /** 文件权限（如 0100644）。 */
    mode?: number;
    /** 修改时间：发送时可为 epoch 秒或 Date，接收时恒为 Date。 */
    mtime?: Date | number;
    files_remaining?: number;
    bytes_remaining?: number;
  }

  export interface SentryOptions {
    /** 非 ZMODEM 数据转发到终端。收到的是字节值数组。 */
    to_terminal: (octets: number[]) => void;
    /** 协议字节发送到远端。收到的是字节值数组。 */
    sender: (octets: number[]) => void;
    /** 检测到新的 ZMODEM 起始序列（ZRQINIT/ZRINIT）。 */
    on_detect: (detection: Detection) => void;
    /** 起始序列被收回（误检测自动回退）。 */
    on_retract: () => void;
  }

  export class Sentry {
    constructor(options: SentryOptions);
    /** 喂入远端输出字节；非 ZMODEM 部分经 to_terminal 转发。 */
    consume(input: Uint8Array | number[] | ArrayBuffer): void;
    get_confirmed_session(): Session | null;
  }

  export class Detection {
    /** 确认开始 ZMODEM 会话，返回 Session（send=远端 rz 收文件 / receive=远端 sz 发文件）。 */
    confirm(): Session;
    /** 拒绝：向远端发送中止序列。 */
    deny(): void;
    is_valid(): boolean;
    get_session_role(): "send" | "receive";
  }

  export class Session {
    type: "send" | "receive";
    /** 发送中止序列（5×CAN + 退格）并结束会话。 */
    abort(): void;
    aborted(): boolean;
    /** 上传侧：发送 ZFIN 结束会话。 */
    close(): Promise<void>;
    on(event: string, callback: (...args: any[]) => void): void;
    /** 上传侧：报价一个文件，远端接受时 resolve Transfer，跳过时 resolve undefined。 */
    send_offer(params: FileDetails): Promise<Transfer | undefined>;
    /** 下载侧：向远端发送 ZRINIT 开始接收。 */
    start(): Promise<unknown>;
  }

  export class Offer {
    get_details(): FileDetails;
    /**
     * 接受下载报价。`on_input` 回调逐子包收到字节值数组；
     * resolve 表示该文件接收完毕（ZEOF）。
     */
    accept(opts?: { on_input?: (octets: number[]) => void; offset?: number }): Promise<unknown>;
    /** 跳过该文件（发送 ZSKIP），继续下一个文件或结束会话。 */
    skip(): void;
    on(event: "input" | "complete", callback: (...args: any[]) => void): void;
  }

  export class Transfer {
    get_details(): FileDetails;
    /** 发送一块文件数据（内部按 8KiB 切 ZMODEM 子包）。 */
    send(array_like: Uint8Array | number[]): void;
    /** 结束该文件传输（发送 ZEOF），resolve 表示远端已确认。 */
    end(array_like?: Uint8Array | number[]): Promise<void>;
  }

  const Zmodem: {
    Sentry: typeof Sentry;
    Session: typeof Session;
    Detection: typeof Detection;
    ZMLIB: {
      ABORT_SEQUENCE: number[];
    };
  };

  export default Zmodem;
}
