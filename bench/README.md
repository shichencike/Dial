# Dial 性能基准套件

同一台机器上，用 **28 个跨语言等价的纯整数微基准**对比 **C / Python / Go / Dial**。
每项在 4 语言必须算出**完全相同的十进制校验和**（112/112）才计入对比——
这是「比值」比较的是等价计算、而非各写一套的正确性锚点。

## 文件

| 文件 | 语言 / 档位 | 说明 |
| --- | --- | --- |
| `bench.c` | C，`gcc -O2` | 28 项实现 + 命令行分派 |
| `bench.go` | Go，`go build` | 同上 |
| `bench.py` | CPython 3，解释执行 | 同上（兼作算法真值定义） |
| `bench.dial` | Dial，`dialc build --fast`（O3 + LTO + SIMD） | 同上 |
| `data.json` | — | 本地校准后的**权威数据快照**（采集器产出，随仓库提交） |

每个程序用法：`<可执行文件/解释器> <任务名> [重复次数]`，只在 stdout 打印一行校验和。
任务名 `b01` … `b28`，另有 `noop`（空任务，测启动基线）。
`重复次数` 供采集器自校准，用于摊薄进程启动开销；重复调用纯函数式任务，校验和恒定。

配套工具（在 `tools/` 下）：

- `bench_harness.py` —— 编译 4 语言、外部统一墙钟计时、校准法测绝对值、
  算绝对比值、采集 6 大工程维度，产出 `data.json`。
- `build_bench_web.py` —— 由 `data.json` 生成独立静态页 `docx/bench.html`
  （绝对值表 + 比值表 + 六维表 + 底部六维 SVG 雷达图）。
- `check_bench_data.py` —— 校验数据完整性与网页/数据同步（CI 与本地共用）。

## 复现

```bash
# 在仓库根目录，需 gcc / go / python3 与已编译的 target/release/dialc
python3 tools/bench_harness.py --dialc target/release/dialc
python3 tools/build_bench_web.py
python3 tools/check_bench_data.py
```

采集耗时约 2 分钟（校准法需多次迭代）。快速冒烟可加 `--quick`（`rep=1`、少采样），
但其绝对值不用于正式对比。CI 的 `build-linux` 作业每次实跑完整采集并上传
`bench-4lang-28dim` artifact，产物写 `/tmp` 不回写仓库网页。

## 一致性铁律

1. 全程值域 `[0, 2^63)`，模数恒定且**被取模/相除的值恒非负**；
2. 整数除法只对正数（C / Go / Dial 向零截断 == Python `//`）；
3. 位运算只在非负且 `< 2^62` 的值上做，符号位不参与。

极快项或被编译器常量折叠成空操作的项（如 C `-O2` 折叠 `b20` 字符串拼接）
统一钳到 5µs 计时下限，网页以 `†` 标注，避免 `0.000ms` 与比值虚高。

详见 [README §36](../README.md) 与 `docx/bench.html` 的「测量口径」。
性能数值随机器与负载浮动，`data.json` 是某次实测快照而非固定承诺值。
