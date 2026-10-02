<p align="center">
  <img src="crates/nkg-desktop/assets/nkg-icon-master.png" alt="NKG Uni Text Edit" width="360" />
</p>

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
- 搜索命中以 8 B/条写入有硬配额的临时磁盘索引，底部虚拟列表按需显示上下文并支持点击跳转；
- 每次搜索保留为独立结果组，可分别收起、展开或删除，便于并排核对多组搜索；
- 支持选择任意两组搜索结果进行双栏对比，既可对比同一文件的不同搜索，也可跨文件对比；
- 正文、搜索结果和搜索结果对比均支持底部横向滚动条浏览超宽行；
- 可见窗口匹配用背景色叠加，不覆盖原有语法颜色；普通文本按日志级别、键、数字和引号内容做轻量着色；
- 支持把一个或多个文件直接拖放到窗口中打开；
- 区段数量有上限、缓冲区大小固定的块级对比概览；
- 仅针对可见窗口的精确行级对比；
- VS Code 风格原生桌面界面：多标签、搜索侧栏、字节跳转和双栏对比；
- 补丁式编辑模式：修改量决定内存占用，后台流式保存为新副本，不原位覆盖源文件；
- `PageUp` / `PageDown` 按当前可见行数上下翻页；
- JSON 语法着色、后台流式结构索引、对象/数组层级、面包屑与 Key 跳转；
- 单行 JSON 自动在后台流式格式化为临时规范视图，不修改原文件；
- XML 语法着色、单行自动格式化、后台元素树、名称筛选、面包屑与节点跳转；
- C/C++ 风格二进制模板导入、后台有界解析与字段树展示，支持字段偏移/大小/类型/值查看和点击跳转；
- 同类型 `.json`/`.xml` 文件自动使用规范化结构对比，忽略排版差异；其他文件仍使用文本对比；
- `nkg-text` 命令行验收工具。

## 快速开始

```powershell
cargo test --workspace
cargo build --release --workspace --locked
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

桌面程序的可执行文件名为 `nkg-text-viewer`。`Ctrl+E` 切换编辑模式，
`Ctrl+Shift+S` 把原文件与补丁流式合并为新副本。主文本区呈现为一个连续文档，界面只保留
一个全文件垂直滚动条，底层窗口换入不会显示成“上一页/下一页”。搜索结果同样使用单个
连续滚动列表，其中每次搜索形成一个类似 Notepad++ 的可折叠结果组。编辑仅记录完整
UTF-8 行的替换补丁；超长行片段和损坏的 UTF-8 行保持只读，避免隐式破坏原始字节。

打开 `.json` 或 `.xml` 文件后，如果检测到整个文件没有换行，会在后台生成临时缩进视图，
再建立行号和结构索引；原文件不会被修改。结构侧栏支持层级浏览、名称筛选和跳转，正文顶部
显示选中节点的准确面包屑，跳转目标行保持高亮。结构索引不设置固定节点数或标签池总量
上限，而是按系统可用内存分块增长；同级节点按页渲染，筛选结果虚拟化显示，因此超过百万
节点后仍可继续索引和访问。只有系统无法继续分配内存时才会明确报错。自动格式化与结构
索引均采用流式处理，不设置固定输入大小上限；结构化对比仍仅用于不超过 256 MiB 的
输入，超限时回退到文本对比。搜索、行号索引和结构均对应当前格式化视图；保存编辑副本
并重新打开后会重建。

从“文件对比”选择同类型 JSON 或 XML 时，程序先在后台生成归一化临时视图，再执行全文件
差异概览和可见窗口精确对比。JSON 对比忽略结构外空白，XML 对比忽略排版空白、注释、声明
以及属性顺序；这属于面向差异查看的归一化，并非完整 XML C14N。源文件不被改写；扩展名
不同、普通文本或超过结构化对比上限的文件继续使用原始文本对比。

从左侧“模”入口可为当前文件导入 `.bt`、`.hexpat`、`.h`、`.hpp` 或文本模板。当前兼容
C/C++ 风格二进制模板的常用顺序布局子集：`struct`、`typedef`、`using`、`enum`，固定宽度
整数、`float`/`double`、`char`/`bool`，定长数组、引用前序整数字段或常量的计数数组，
`#pragma endian little|big`、`LittleEndian()`/`BigEndian()`，以及 ImHex 风格的顶层
`Type value @ 0xOFFSET;`。例如：

```cpp
#pragma endian little
struct Header {
    uint32_t magic;
    uint16_t count;
    char name[16];
    uint32_t offsets[count];
};
Header header;
```

模板按二进制模板语义紧凑顺序解析，不套用编译器 ABI 的隐式内存对齐。模板源限制为
`1 MiB`，单数组最多 `100,000` 项，单次结果最多 `200,000` 个节点，避免异常模板耗尽
内存；暂不执行模板中的指针、位字段、函数、循环或任意表达式。

桌面端每个文档最多保留 8 组搜索会话；单次搜索不限制结果数量，所有命中持续写入临时磁盘
索引，空间不足时会作为明确的 I/O 错误报告。预览只缓存可见结果附近的 512 条，并限制单条
读取与行首回扫。

创建超大稀疏验收文件（目标路径必须不存在）：

```powershell
cargo run -p nkg-cli -- fixture .\tmp\100g.txt --size-gib 100
cargo run -p nkg-cli -- inspect .\tmp\100g.txt --offset 107374182000
```

详细约束与验收标准见 [docs/architecture.md](docs/architecture.md)。
真实 Unity 日志的基准结果见 [docs/benchmarks.md](docs/benchmarks.md)。
