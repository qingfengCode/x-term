<!--
  ResultTableV2.vue — SQL 查询结果虚拟滚动表格（el-table-v2 封装）。

  背景：el-table 全量渲染 DOM，大结果集（数万行）整页卡顿；el-table-v2
  虚拟滚动只渲染可视行，10 万行流畅。命令行模式与代码模式的结果表共用。

  数据结构：rows 为 string[][]（每行按列下标取值），见 useSqlConsole。
  行高固定 34px，单元格省略 + title 悬浮全文。
-->
<script setup lang="ts">
import { computed } from "vue";
import type { Column } from "element-plus";

const props = defineProps<{
  /** 行数据：每行为按列下标的值数组。 */
  rows: string[][];
  /** 列名（顺序即展示顺序）。 */
  columns: string[];
  /** 表格高度（px；虚拟滚动需确定高度）。 */
  height: number;
  /** 数据列最小宽度。 */
  minWidth?: number;
}>();

/** el-table-v2 列定义（数据驱动）：首列行号 + 数据列（按列下标取值）。
    单元格内容统一由 #cell 插槽渲染（含行号）。 */
const v2Columns = computed<Column<any>[]>(() => {
  const cols: Column<any>[] = [
    { key: "__idx", dataKey: "__idx", title: "#", width: 60, align: "center" },
  ];
  props.columns.forEach((c, i) => {
    cols.push({
      key: String(i),
      dataKey: String(i),
      title: c,
      width: props.minWidth ?? 140,
      flexGrow: 1,
    });
  });
  return cols;
});
</script>

<template>
  <div class="result-table-v2" :style="{ height: props.height + 'px' }">
    <el-auto-resizer>
      <template #default="{ width }">
        <el-table-v2
          :columns="v2Columns"
          :data="props.rows"
          :width="width"
          :height="props.height"
          :row-height="34"
          :header-height="36"
          fixed
        >
          <template #cell="{ column, rowData, rowIndex }">
            <span v-if="column.key === '__idx'" class="cell-idx">{{ rowIndex + 1 }}</span>
            <span
              v-else
              class="cell-text"
              :title="String((rowData as string[])[Number(column.key)] ?? '')"
            >{{ (rowData as string[])[Number(column.key)] }}</span>
          </template>
        </el-table-v2>
      </template>
    </el-auto-resizer>
  </div>
</template>

<style scoped lang="scss">
.result-table-v2 {
  width: 100%;
  overflow: hidden;
}
.cell-text {
  display: block;
  overflow: hidden;
  text-overflow: ellipsis;
  white-space: nowrap;
  line-height: 34px;
  font-size: 12px;
}
.cell-idx {
  color: var(--el-text-color-placeholder);
  font-size: 12px;
  font-family: var(--el-font-family-mono, monospace);
}
</style>
