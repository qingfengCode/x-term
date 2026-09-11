import { nextTick, onBeforeUnmount, ref, watch, type Ref } from "vue";
import { EditorState, StateEffect } from "@codemirror/state";
import { EditorView, keymap, lineNumbers } from "@codemirror/view";
import { defaultKeymap, history, historyKeymap } from "@codemirror/commands";
import { oneDark } from "@codemirror/theme-one-dark";
import { sql, MySQL, PostgreSQL, SQLConfig } from "@codemirror/lang-sql";
import { Compartment } from "@codemirror/state";

/**
 * CodeMirror v6 + Vue 3 整合 composable。
 *
 * CM 自管 DOM：onMounted 时挂到容器 div，onBeforeUnmount 调 view.destroy()。
 * 通过 updateListener 把文档同步到外部 ref；外部 ref 变化时（程序设值）也写回 CM。
 *
 * @param container 容器元素 ref
 * @param model 双向绑定的文本 ref
 * @param tables 表→字段映射（用于 SQL 自动补全：表名 + 字段名）；变化时重配语言扩展
 * @param onCtrlEnter Ctrl+Enter 回调（执行 SQL）
 * @param isDark 主题暗色标记
 * @param onEnter Enter 回调
 * @param dialect SQL 方言（mysql / postgres / sqlite，默认 postgres）；切换标签时变化重配高亮/补全
 */
export function useCodeMirror(
  container: Ref<HTMLElement | null>,
  model: Ref<string>,
  tables: Ref<Record<string, string[]>>,
  onCtrlEnter: () => void,
  isDark: Ref<boolean>,
  onEnter?: () => void,
  dialect?: Ref<"mysql" | "postgres" | "sqlite">,
) {
  let view: EditorView | null = null;
  // 用 Compartment 让语言扩展（含 tables / dialect）可在运行时重配。
  const langCompartment = new Compartment();
  const themeCompartment = new Compartment();

  /** 当前方言（未传 dialect 参数时按 postgres）。
   *  mount / watch / buildExtensions 共用，保证取值一致。
   *  SQLite 用 PostgreSQL 方言近似（@codemirror/lang-sql 无 SQLite 方言）。 */
  type EditorDialect = "mysql" | "postgres";
  const currentDialect = (): EditorDialect =>
    dialect?.value === "mysql" ? "mysql" : "postgres";

  function buildSqlExtension(
    schemaMap: Record<string, string[]>,
    lang: EditorDialect = "postgres",
  ): SQLConfig {
    // schema 用 SQLNamespace 形式：{ 表名: { 字段名: {} } }，字段来自 DESCRIBE 预拉。
    // 未预拉字段的表，值为空对象（仍可补全表名，字段补全为空）。
    const tableMap: Record<string, Record<string, Record<string, never>>> = {};
    for (const [table, cols] of Object.entries(schemaMap)) {
      const fieldMap: Record<string, Record<string, never>> = {};
      for (const c of cols) fieldMap[c] = {};
      tableMap[table] = fieldMap;
    }
    return {
      upperCaseKeywords: true,
      schema: tableMap,
      // 按连接的数据库类型选方言（关键字/函数高亮与补全）。
      dialect: lang === "mysql" ? MySQL : PostgreSQL,
    };
  }

  function buildExtensions(schemaMap: Record<string, string[]>) {
    return [
      lineNumbers(),
      history(),
      keymap.of([...defaultKeymap, ...historyKeymap]),
      // Ctrl+Enter 执行（备用，主交互由命令行容器的原生 keydown 兜底）。
      keymap.of([
        {
          key: "Ctrl-Enter",
          mac: "Cmd-Enter",
          preventDefault: true,
          run: () => {
            onCtrlEnter();
            return true;
          },
        },
      ]),
      langCompartment.of(sql(buildSqlExtension(schemaMap, currentDialect()))),
      themeCompartment.of(isDark.value ? oneDark : []),
      EditorView.lineWrapping,
      // 文档变化 → 同步到 model ref（避免循环：仅当不同时写）
      EditorView.updateListener.of((u) => {
        if (u.docChanged) {
          const next = u.state.doc.toString();
          if (next !== model.value) model.value = next;
        }
      }),
    ];
  }

  function mount() {
    if (!container.value || view) return;
    view = new EditorView({
      state: EditorState.create({
        doc: model.value,
        extensions: buildExtensions(tables.value),
      }),
      parent: container.value,
    });
  }

  /** 销毁后重新挂载（用于容器 DOM 因 v-if 切换而变化时）。 */
  function remount() {
    destroy();
    nextTick(() => mount());
  }

  function destroy() {
    if (view) {
      view.destroy();
      view = null;
    }
  }

  // tables 变化时重配 SQL 扩展（补全列表更新）。方言必须随当前取值重传：
  // 漏传会回退 buildSqlExtension 的默认 postgres，MySQL 标签展开库节点
  // （tables 刷新触发本 watch）后高亮/补全方言被重置。
  watch(
    tables,
    (list) => {
      if (!view) return;
      view.dispatch({
        effects: langCompartment.reconfigure(sql(buildSqlExtension(list, currentDialect()))),
      });
    },
    { deep: true },
  );

  // 方言切换（同容器复用、不重挂载的场景）同样重配语言扩展。
  if (dialect) {
    watch(dialect, () => {
      if (!view) return;
      view.dispatch({
        effects: langCompartment.reconfigure(
          sql(buildSqlExtension(tables.value, currentDialect())),
        ),
      });
    });
  }

  // 主题切换。
  watch(isDark, (dark) => {
    if (!view) return;
    view.dispatch({ effects: themeCompartment.reconfigure(dark ? oneDark : []) });
  });

  // 外部程序设值（非用户输入）→ 写回 CM。
  watch(model, (val) => {
    if (view && val !== view.state.doc.toString()) {
      view.dispatch({
        changes: { from: 0, to: view.state.doc.length, insert: val },
      });
    }
  });

  onBeforeUnmount(destroy);

  return { mount, remount, destroy, getView: () => view };
}
