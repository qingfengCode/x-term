/**
 * @novnc/novnc 最小类型声明（官方包不附带 TypeScript 类型）。
 * 仅覆盖本项目用到的 API，与 noVNC 1.7 的实际实现对齐。
 */
declare module "@novnc/novnc" {
  /** RFB 构造选项（noVNC 1.7）。 */
  export interface RFBOptions {
    /** VNC 认证凭据；密码由 RFB 握手时使用，不经过后端桥接。 */
    credentials?: { username?: string; password?: string };
    /** 与已存在的会话共享（默认 true）。 */
    shared?: boolean;
    /** 缩放到容器尺寸（随容器 resize 自动重算）。 */
    scaleViewport?: boolean;
    /** 连接后向服务端请求桌面尺寸调整（需服务端支持）。 */
    resizeSession?: boolean;
    wsProtocols?: string[];
  }

  export default class RFB {
    constructor(target: HTMLElement, url: string, options?: RFBOptions);
    /** 缩放到容器（true 时随容器尺寸自动重算）。 */
    scaleViewport: boolean;
    /** 点击画布时抓取键盘焦点。 */
    focusOnClick: boolean;
    /** 请求服务端把桌面分辨率调整为窗口尺寸（需服务端支持 ExtendedDesktopSize）。 */
    resizeSession: boolean;
    /** JPEG 质量级别 0-9（默认 6；越低画质越差、带宽越低）。 */
    qualityLevel: number;
    /** zlib 压缩级别 0-9（默认 2；越低压缩越弱、CPU 越省）。 */
    compressionLevel: number;
    addEventListener(type: "connect" | "disconnect" | "credentialsrequired" | "securityfailure", callback: (e: CustomEvent<{ clean?: boolean; detail?: { status?: number; reason?: string } }>) => void): void;
    removeEventListener(type: string, callback: (e: CustomEvent) => void): void;
    /** 断开连接。 */
    disconnect(): void;
    /** 认证握手要求凭据时提交（macOS 屏幕共享 ARD 需 username+password）。 */
    sendCredentials(credentials: { username?: string; password?: string; target?: string }): void;
    /** 发送 Ctrl+Alt+Del 组合键（Windows VNC 解锁登录屏常用）。 */
    sendCtrlAltDel(): void;
    /** 把本地剪贴板文本粘贴给远端（ClientCutText）。 */
    clipboardPasteFrom(text: string): void;
    sendKey(keysym: number, code: string, down: boolean): void;
  }
}
