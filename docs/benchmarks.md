# 性能验收记录

## 2026-07-31 优化复测

在同一台机器上使用固定的 256 MiB 合成日志，分别从优化前 `HEAD` 和当前工作树构建 release
二进制。每项操作先预热，再按“旧版/新版”交错执行 11 轮并取中位数，降低缓存温度和系统
负载漂移造成的偏差。以下数据包含 CLI 进程，但不包含编译时间。

| 操作 | 优化前中位数 | 优化后中位数 | 提升 | 优化后峰值工作集 |
| --- | ---: | ---: | ---: | ---: |
| 无命中字面量搜索 | 64.26 ms | 62.16 ms | 3.3% | 13.18 MiB |
| 密集 `search-all` | 119.42 ms | 106.12 ms | 11.1% | 13.27 MiB |
| 完整稀疏行索引 | 119.53 ms | 74.19 ms | 37.9% | 20.07 MiB |
| 同一路径全量对比 | 74.04 ms | 0.07 ms | 99.9% | 3.02 MiB |
| 不同文件 ID、相同内容对比 | 120.04 ms | 118.14 ms | 1.6%（近噪声） | 12.54 MiB |

密集搜索产生 2,130,432 个命中，磁盘索引为 17,043,456 bytes，即精确的 8 bytes/命中。
“不同文件 ID、相同内容”样本的 SHA-256 相同，但 NTFS file ID 不同，因此不会命中同文件
零 I/O 快速路径。

同一版本还通过了 100 GiB NTFS 稀疏文件的首部、中点和尾部验收：打开耗时
0.122–0.143 ms，4 KiB 精确窗口读取 0.092–0.302 ms。稀疏文件用于验证 `u64` 偏移和
分配上界，不代表实体 100 GiB 文件的磁盘吞吐。

当前版本的标准复测命令：

```powershell
cargo build --release --workspace --locked
cargo run --release --locked -p nkg-cli -- inspect "<LOG>" --offset 130466916
cargo run --release --locked -p nkg-cli -- search "<LOG>" "__NKG_PATTERN_THAT_DOES_NOT_EXIST__" --max-results 1
cargo run --release --locked -p nkg-cli -- search-all "<LOG>" "Unity" --sample-results 3
cargo run --release --locked -p nkg-cli -- index "<LOG>" --probe-offset 130466916
cargo run --release --locked -p nkg-cli -- compare "<LOG>" "<LOG>"
```

## 2026-07-29 真实 Unity 日志记录（优化前）

测试日期：2026-07-29。测试文件是一份 Unity `Editor_Pre.log`，只读访问，未复制或修改。

## 环境

- Windows 11 专业版 64 位（10.0.26200）；
- Intel Core Ultra 9 285K，24 核 / 24 逻辑处理器；
- 127.4 GiB 内存；
- KIOXIA KXG80ZN84T09，E: 为 NTFS；
- Rust 1.97.1；
- 文件大小：260,933,833 bytes（248.85 MiB）；
- 总行数：2,144,921。

以下结果来自同一轮开发期间的暖缓存测试，仅用于检查复杂度和内存上界，不作为不同硬件
之间的吞吐量承诺。

## 结果

| 操作 | 结果 | 峰值工作集 |
| --- | ---: | ---: |
| 打开文件（仅元数据） | 约 0.12 ms | 包含在进程基线中 |
| 开头 / 中部 / 文件尾窗口读取 | 约 0.15–0.30 ms | 有界窗口 |
| 不存在字面量的整文件搜索 | 796.41 ms | 16.20 MiB |
| `Unity` 全部 994,281 个命中（调试构建，含磁盘索引） | 1.11 s | 16.61 MiB |
| `Unity` 全部 994,281 个命中（发布构建，暖缓存） | 85.24 ms | 未单独采样 |
| 完整稀疏行索引 | 707.07 ms | 19.95 MiB |
| 同文件全量块级对比 | 74.88 ms | 13.87 MiB |
| 原生桌面自对比冒烟测试（调试构建） | 始终响应 | 约 128 MiB |
| 原生桌面搜索冒烟测试（发布构建） | 始终响应 | 121.84 MiB |

中点字节偏移 `130,466,916` 在索引完成后定位为第 `1,079,393` 行。稀疏索引共保存
33 个检查点。桌面进程的工作集包含 OpenGL 驱动、窗口系统和中文字体；核心命令行结果
更能反映随文件大小变化的算法内存。

`Unity` 搜索产生 994,281 个命中，旧版磁盘结果索引为 15,908,496 bytes（当时每条
16 bytes），没有 10,000 条截断。当前实现已改为每条 8 bytes，并加入显式磁盘配额。

## 当时的操作形状

```powershell
cargo run --release --locked -p nkg-cli -- inspect "<LOG>" --offset 130466916
cargo run --release --locked -p nkg-cli -- search "<LOG>" "__NKG_PATTERN_THAT_DOES_NOT_EXIST__" --max-results 1
cargo run --release --locked -p nkg-cli -- search-all "<LOG>" "Unity" --sample-results 3
cargo run --release --locked -p nkg-cli -- index "<LOG>" --probe-offset 130466916
cargo run --release --locked -p nkg-cli -- compare "<LOG>" "<LOG>"
cargo run --release --locked -p nkg-desktop -- "<LOG>" "<LOG>"
```

冷缓存、机械盘、网络盘和实时增长日志需要单独记录；这些环境下延迟会变化，但核心扫描
缓冲区和单次 UI 读取窗口仍保持固定大小。当前桌面端达到结果磁盘配额时会明确停止并提示，
磁盘索引占用按每条 8 bytes 线性增长。
