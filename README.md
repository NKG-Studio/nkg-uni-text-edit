# NKG Uni Text Edit

面向超大文本文件的桌面阅读、搜索、高亮与对比工具。目标是在普通 64 位桌面环境中处理
上百 GB 文件时，内存占用不随文件大小线性增长。

项目采用“独立 Rust 核心 + 原生 Rust 桌面界面”的结构。桌面层只能请求有限大小的
可见窗口，不能把完整文件送入 UI 或编辑缓冲区。

## 当前能力

- `u64` 文件偏移，打开文件时只读取元数据；
- 按字节窗口读取，可对齐到包含指定偏移的行；
- 后台渐进式稀疏行索引；
- 连续虚拟文档视图：滚动时无缝换入有界窗口，不向用户显示分页；
- 常量缓冲区、可取消的字节流搜索；
- 全部搜索命中写入临时磁盘索引，底部虚拟列表显示上下文并支持点击跳转；
- 每次搜索保留为独立结果组，可分别收起、展开或删除，便于并排核对多组搜索；
- 可见窗口匹配高亮；
- 支持把一个或多个文件直接拖放到窗口中打开；
- 区段数量有上限、缓冲区大小固定的块级对比概览；
- 仅针对可见窗口的精确行级对比；
- VS Code 风格原生桌面界面：多标签、搜索侧栏、字节跳转和双栏对比；
- `nkg-text` 命令行验收工具。

## 快速开始

```powershell
cargo test --workspace
cargo build --release --workspace
cargo run -p nkg-cli -- inspect README.md
cargo run -p nkg-cli -- search README.md Rust
cargo run -p nkg-desktop -- "E:\path\to\large.log"
cargo run -p nkg-desktop -- "E:\path\to\large.log" --search "needle"
```

发布构建完成后可直接运行：

```powershell
.\target\release\nkg-text-viewer.exe "E:\path\to\large.log" --search "needle"
```

直接进入双栏对比：

```powershell
cargo run -p nkg-desktop -- "E:\path\to\left.log" "E:\path\to\right.log"
```

桌面程序的可执行文件名为 `nkg-text-viewer`。主文本区呈现为一个连续文档，界面只保留
一个全文件垂直滚动条，底层窗口换入不会显示成“上一页/下一页”。搜索结果同样使用单个
连续滚动列表，其中每次搜索形成一个类似 Notepad++ 的可折叠结果组。界面和核心目前均
为只读，避免在超大日志上误触发整体重写；后续编辑能力会采用独立的补丁层实现。

创建超大稀疏验收文件（目标路径必须不存在）：

```powershell
cargo run -p nkg-cli -- fixture .\tmp\100g.txt --size-gib 100
cargo run -p nkg-cli -- inspect .\tmp\100g.txt --offset 107374182000
```

详细约束与验收标准见 [docs/architecture.md](docs/architecture.md)。
真实 Unity 日志的基准结果见 [docs/benchmarks.md](docs/benchmarks.md)。
