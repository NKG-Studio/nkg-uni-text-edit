# 真实日志验收记录

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

`Unity` 搜索产生 994,281 个命中，磁盘结果索引为 15,908,496 bytes（每条 16 bytes），
没有 10,000 条截断。

## 可重复命令

```powershell
cargo run -p nkg-cli -- inspect "<LOG>" --offset 130466916
cargo run -p nkg-cli -- search "<LOG>" "__NKG_PATTERN_THAT_DOES_NOT_EXIST__" --max-results 1
cargo run -p nkg-cli -- search-all "<LOG>" "Unity" --sample-results 3
cargo run -p nkg-cli -- index "<LOG>" --probe-offset 130466916
cargo run -p nkg-cli -- compare "<LOG>" "<LOG>"
cargo run -p nkg-desktop -- "<LOG>" "<LOG>"
```

冷缓存、机械盘、网络盘和实时增长日志需要单独记录；这些环境下延迟会变化，但核心扫描
缓冲区和单次 UI 读取窗口仍保持固定大小。全部命中不会截断，磁盘索引占用按每条 16 bytes
线性增长。
