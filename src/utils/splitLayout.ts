/**
 * 终端分屏布局树（每个终端页签内一棵）。
 *
 * 叶子节点引用窗格 id（窗格连接状态在 terminals store 的
 * `TerminalPaneState` 中维护）；分支节点按方向二分，`ratio` 为第一个
 * 子树的空间占比（0.1 ~ 0.9，拖拽分隔条调节）。
 *
 * 树操作均为纯函数：split 在目标叶子处展开为分支（新窗格作为第二个
 * 子节点，出现在右侧 / 下侧）；remove 移除叶子后兄弟子树上提 collapsing
 * 父分支。调用方用返回值重新赋值 `tab.layout`（根节点可能被替换）。
 */

/** 分屏方向：row = 左右并排（垂直分隔线）；col = 上下堆叠（水平分隔线）。 */
export type SplitDirection = "row" | "col";

export interface SplitLeafNode {
  type: "leaf";
  /** 引用的窗格 id（tab.panes 中的 TerminalPaneState.id）。 */
  paneId: string;
}

export interface SplitBranchNode {
  type: "branch";
  dir: SplitDirection;
  /** 第一个子树的空间占比（0.1 ~ 0.9）。 */
  ratio: number;
  children: [SplitNode, SplitNode];
}

export type SplitNode = SplitLeafNode | SplitBranchNode;

/**
 * 在目标叶子处拆分：叶子升级为分支，原叶子成为第一个子节点、新窗格叶子
 * 成为第二个子节点（新窗格出现在右侧 / 下侧）。未命中目标时原样返回。
 */
export function splitLeafNode(
  root: SplitNode,
  targetPaneId: string,
  dir: SplitDirection,
  newPaneId: string,
): SplitNode {
  if (root.type === "leaf") {
    if (root.paneId !== targetPaneId) return root;
    return {
      type: "branch",
      dir,
      ratio: 0.5,
      children: [root, { type: "leaf", paneId: newPaneId }],
    };
  }
  root.children[0] = splitLeafNode(root.children[0], targetPaneId, dir, newPaneId);
  root.children[1] = splitLeafNode(root.children[1], targetPaneId, dir, newPaneId);
  return root;
}

/**
 * 移除目标叶子：兄弟子树上提 collapsing 父分支。返回新的根节点；树被
 * 清空（只剩目标叶子）时返回 null（由调用方保证不会发生——最后一个窗格
 * 关闭走整页关闭）。
 */
export function removeLeafNode(root: SplitNode, targetPaneId: string): SplitNode | null {
  if (root.type === "leaf") {
    return root.paneId === targetPaneId ? null : root;
  }
  const a = removeLeafNode(root.children[0], targetPaneId);
  const b = removeLeafNode(root.children[1], targetPaneId);
  if (!a && !b) return null;
  if (!a) return b;
  if (!b) return a;
  root.children = [a, b];
  return root;
}

/** 按布局顺序收集全部叶子窗格 id（窗格间导航的顺序基准）。 */
export function collectPaneIds(root: SplitNode): string[] {
  if (root.type === "leaf") return [root.paneId];
  return [...collectPaneIds(root.children[0]), ...collectPaneIds(root.children[1])];
}

/** 子树是否包含指定窗格（最大化窗格时分支只保留包含它的孩子）。 */
export function subtreeHas(root: SplitNode, paneId: string): boolean {
  if (root.type === "leaf") return root.paneId === paneId;
  return subtreeHas(root.children[0], paneId) || subtreeHas(root.children[1], paneId);
}
