# Broforce 修改器

一键把 Broforce 的生命值改成 999。Rust + egui 写的 Windows 桌面小工具，单个 exe、不需要注入 DLL。


## 用法

1. 启动 Broforce，**进入关卡**（不在关卡里玩家对象不存在，改不了）
2. 运行 `BroforceTrainer.exe`
3. 点「连接」
4. 点「设为 999 生命」

改之前建议先点一下「检测当前生命」——它会**只读地**定位生命值并显示当前数值。这个数和你屏幕上的一致，才说明瞄对了。窗口默认置顶，游戏全屏时也点得到。

## 原理

Broforce 是 Unity 5 + Mono 构建的（x64，**不是** IL2CPP），游戏目录里带着完整的 `mono.dll`，导出 806 个 C API。所以不用猜内存布局，可以直接问运行时。游戏程序集里有关键的几项：

```
HeroController.SetLives(int playerNum, int livesCount)   // static
HeroController.GetPlayerLives(int playerNum) -> int      // static
HeroController.players : Player[]                        // static
Player.lives : int                                       // instance
```

`src/trainer.rs` 里分三步：

**① 查元数据（只读）**
在游戏进程里 `VirtualAllocEx` 一小块可执行内存，写进一段手写的 x64 stub，用 `CreateRemoteThread` 跑它。stub 用 mono API 依次取到：

```
mono_image_loaded("Assembly-CSharp")
  → mono_class_from_name(..., "HeroController")
  → mono_class_get_field_from_name(..., "players")
  → mono_field_get_offset / mono_class_vtable / mono_vtable_get_static_field_data
```

结果写回内存块里的槽位，由我们这边读走。stub 每一步后面都有 `test rax, rax / jz` 守卫——把空指针喂给 mono 会让它直接崩掉。

**② 定位（只读）**
本进程读出 `players` 数组 → 取出玩家对象 → 加上 `lives` 的字段偏移。对象有没有找对，用「Mono 对象的前 8 字节就是它所属类的 vtable」这条判据校验。

**③ 写入**
`WriteProcessMemory` 写 4 个字节。

全程**不执行任何游戏逻辑**，HUD 会在主线程的下一帧自己读到新值。

### 为什么不用内存扫描

最初就是按 Cheat Engine 那套做的（扫描 → 用户筛选 → 锁定），实测不可行：

- 游戏有 **1.9 GB** 可写内存，全量扫描一次 **24 秒**起步
- 更糟的是游戏运行时内存区间随时失效，读失败的兜底逻辑会退化成「每次只前进 4 字节」的爬行，实测有一次跑了 **369 秒**
- 而且要求用户自己输当前命数、反复筛选——把「找到变量」这个本该由程序解决的问题推给了用户

### 为什么不直接调用游戏的 `SetLives`

`HeroController.SetLives` 是现成的 setter，远程调它语义最干净，但**会把游戏搞崩**。实测崩了两次，两次都是同一个确定性错误：

```
出错模块: UnityPlayer.dll     异常代码: 0xc0000005（访问冲突）
错误偏移: 0x000000000081029C
```

`SetLives` 会去刷新 HUD，而 **Unity 的 API 只能在主线程调用**——我们的远程线程不是 Unity 线程。所以改成只查元数据、由本进程直接写内存。

### Mono 数组的布局

实测这个 Unity 自带的 Mono 里，数组是 16 字节对象头，**长度在 `+0x18`，元素从 `+0x20` 开始**：

```
+00  vtable
+08  synchronisation
+10  0
+18  长度 = 4
+20  元素[0]  ← Player 对象
```

这跟「长度紧跟对象头、在 `+0x10`」的常见假设不一样，踩过一次坑。代码里两个候选偏移都会试，最终由 vtable 校验定夺——布局判断过时了也只会定位失败，不会认错对象乱写。

## 已知限制

- **只支持普通模式的命数**（`Player.lives`）。硬核模式走的是 `PlayerProgress.hardcoreLives`，不支持。
- **仅限单人**。联机下会改到其他玩家。
- **必须已进入关卡**，`players[0]` 存在时才改得动。
- 游戏更新换了 `mono.dll` 之后 RVA 会变——所以是运行时解析 PE 导出表，不是写死地址。
- 如果游戏以管理员身份启动，本程序也需要管理员权限（`OpenProcess` 会失败并给出提示）。

## 从源码构建

```bash
cargo build --release
# 产物：target/release/broforce.exe
```

测试：

```bash
cargo test                 # 19 项单测：stub 机器码编码、栈对齐、跳转回填、PE 导出解析、真实进程里的远程执行
cargo test -- --ignored    # 需要 Broforce 正在运行（只读，不改游戏）
```

## 项目结构

```
src/
  main.rs      eframe 入口
  app.rs       egui 界面
  trainer.rs   业务逻辑：定位 life 地址、写入
  inject.rs    x64 stub 生成 + 远程执行
  mono.rs      定位 mono.dll、解析导出、算函数地址
  pe.rs        PE 导出表解析
  win32.rs     进程 / 内存 / 远程线程 API 封装
```

## 发版

推一个 `v*` 标签即可，GitHub Actions 会自动跑测试、构建并创建 Release：

```bash
git tag v0.1.0
git push origin v0.1.0
```

也可以在 Actions 页面手动触发（`workflow_dispatch`），那样只产出 artifact，不发 Release。
