# keyword-demo：引用上传插件的题目包示例

1. 上传插件：`crucible plugin upload examples/plugins/keyword-scorer --wait`，记下打印的 `u-...`。
2. 把 `source.json` 里的 `u-0000000000000000` 换成这个 id。
3. 上传题目包：`crucible taskset upload examples/tasksets/keyword-demo --wait`，记下题目包 id。
4. app 模式提交一份产出（zip 根目录有 `answer.md`）：`crucible submit app --taskset <题目包 id> --stage 1 --zip answer.zip --i-agree --wait`。

答案里出现 `agent`、`container`、`leaderboard` 各得 1 分，满分 3。
