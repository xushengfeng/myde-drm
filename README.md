# myde-drm

把 Linux DRM dma-buf 的 32 位像素格式转换成紧密排列的 RGBA `Buffer`，供 Wayland 合成器（myde）在 Electron 不支持某些格式时使用。

Electron 的 `sharedTexture.importSharedTexture` 只接受 `bgra`、`rgba`、`rgbaf16`、`nv12`、`nv16`、`p010le`。Chromium 内部其实认识 XRGB/XBGR，但 JS 接口没有暴露，而且 XRGB/XBGR 的 alpha 字节是未定义的。本模块把这些格式统一读出来，按 RGBA 输出。
- Rust 原生实现（neon），通过 mmap 读取 dma-buf
- TypeScript 包装，vite 构建，pnpm 管理
- 只支持 **LINEAR** modifier（当前 myde 的 dmabuf feedback 只广播 modifier 0）
- 只处理单 plane 的 32 位 RGB(A) 格式；NV12/NV16/P010 请继续走 Electron 原生的 `importSharedTexture`

## 构建

```bash
pnpm install
pnpm build        # cargo build --release + vite build
pnpm test
pnpm typecheck
```

构建产物：`index.node`（Rust 原生模块）、`dist/index.js`、`dist/index.cjs`、`dist/index.d.ts`。

## 用法

### 1. 先判断格式能否直接交给 Electron

```ts
import { getElectronPixelFormat } from "myde-drm";

const pixelFormat = getElectronPixelFormat(bufferObj.format);
if (pixelFormat) {
    // 保持原来的 importSharedTexture 路径
    // ARGB8888 -> bgra, ABGR8888 -> rgba, NV12/NV16/P010 原样传
} else {
    // 需要转换，例如 XRGB8888 / XBGR8888 / RGBX8888 / BGRX8888 / RGBA8888 / BGRA8888
}
```

### 2. 转换 dmabuf 到 RGBA Buffer

```ts
import { convertDmabufToRgba, DRM_FORMAT } from "myde-drm";

const rgba = convertDmabufToRgba({
    width: bufferObj.width,
    height: bufferObj.height,
    format: bufferObj.format,
    modifier: 0n, // 只接受 DRM_FORMAT_MOD_LINEAR
    planes: [
        {
            fd: bufferObj.planes[0].fd,
            offset: bufferObj.planes[0].offset,
            stride: bufferObj.planes[0].stride,
        },
    ],
});

const image = new ImageData(
    new Uint8ClampedArray(rgba.buffer, rgba.byteOffset, rgba.byteLength),
    bufferObj.width,
    bufferObj.height,
);
ctx.putImageData(image, 0, 0);
```

`convertDmabufToRgba` 返回的 Buffer 长度是 `width * height * 4`，行间没有 padding。

### 3. 只查询格式

```ts
import { getFormatInfo, isSupportedFormat, needsConversion } from "myde-drm";

getFormatInfo(DRM_FORMAT.XRGB8888);
// {
//   name: "DRM_FORMAT_XRGB8888",
//   electronPixelFormat: null,
//   needsConversion: true
// }

isSupportedFormat(DRM_FORMAT.XRGB8888); // true
needsConversion(DRM_FORMAT.ARGB8888); // false
```

## 格式表

下表中的“内存字节”是 little-endian 下每个像素从低地址到高地址的字节顺序。

| DRM fourcc            | 内存字节 | Electron 直接导入 | 转换           |
| --------------------- | -------- | ----------------- | -------------- |
| `DRM_FORMAT_ARGB8888` | B G R A  | `bgra`            | 不需要         |
| `DRM_FORMAT_ABGR8888` | R G B A  | `rgba`            | 不需要         |
| `DRM_FORMAT_XRGB8888` | B G R X  | —                 | 转 RGBA，A=255 |
| `DRM_FORMAT_XBGR8888` | R G B X  | —                 | 转 RGBA，A=255 |
| `DRM_FORMAT_RGBX8888` | X B G R  | —                 | 转 RGBA，A=255 |
| `DRM_FORMAT_BGRX8888` | X R G B  | —                 | 转 RGBA，A=255 |
| `DRM_FORMAT_RGBA8888` | A B G R  | —                 | 转 RGBA        |
| `DRM_FORMAT_BGRA8888` | A R G B  | —                 | 转 RGBA        |
| `DRM_FORMAT_NV12`     | 多平面   | `nv12`            | 不处理         |
| `DRM_FORMAT_NV16`     | 多平面   | `nv16`            | 不处理         |
| `DRM_FORMAT_P010`     | 多平面   | `p010le`          | 不处理         |

## 限制

- modifier 必须是 `DRM_FORMAT_MOD_LINEAR`（0）。tiled buffer 无法直接 mmap，遇到非 0 modifier 会抛错。
- 只支持单 plane 的 32 位格式。NV12/NV16/P010 不做转换。
- `fd` 由调用方拥有，模块只读映射，不会 close。
- 每次转换都会在 CPU 上分配 `width * height * 4` 的新 Buffer，适合不支持格式的回退路径；能直通的格式不要走这里。
