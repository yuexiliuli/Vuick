// BMP C# runner：两个子命令。
//
// `ref <corpusRoot> <refOutDir>` —— 单核参照测量：对语料目录（<corpusRoot>/BMP +
// <corpusRoot>/BMP-MATRIX 下的每个 *.bmp）用 Sdcb.SimdCodecs.Bmp.BmpImageDecoder
// TryDecodeIntoRgba8 计时（全集暖机 1 遍 + 每文件 warmup2 + N 轮中位，BMP_REF_ROUNDS env 控制），
// 并把该文件的 RGBA8（紧密 4B/px、w*h*4）解码字节 dump 到
// <refOut>/<stem>.<w>x<h>.rgba（bmp_csharp_parity 的字节恒等参照）。
// 表行 `| `name` | ms |` 供 compare_refs.py 消费。
//
// `gen <corpusRoot> <outDir>` —— 矩阵语料生成器：photo 源 A/B（<corpusRoot>/BMP/001.bmp、
// 002.bmp）与合成 screen 源各经 Sdcb BmpEncoder 产出 11 个布局变体；另用极小 hand-rolled
// 低层写入器产 p2（2-bit 调色板）与 rgb565-444（16bit BITFIELDS 444）各 1 份（singles 源）。
// 全部覆盖式重生成到 <outDir>/BMP-MATRIX（文件命名 <source>__<layout>.bmp），输出生成清单日志。
//
// 布局矩阵（镜像 C# 官方 Bmp 基准覆盖，见 cases 矩阵；bgra32 = Argb32+Info 头 → 32bpp
// BI_RGB 带 alpha 第 4 字节，argb32-v4 = Argb32+V4 头 → BI_BITFIELDS alpha 掩码）：
//   rgb24 / rgb24-topdown / bgra32 / argb32-v4 / rgb555 / rgb565 / p8 / p4 / p1 / rle8 / rle4
//   + p2 / rgb565-444（hand-rolled，Sdcb 编码器产不出）

using System.Buffers.Binary;
using System.Diagnostics;
using System.Globalization;
using Sdcb.SimdCodecs.Abstractions;
using Sdcb.SimdCodecs.Bmp;

if (args.Length < 1)
{
    Usage();
    return 2;
}

return args[0] switch
{
    "ref" => RunRef(args),
    "gen" => RunGen(args),
    _ => UsageReturn(),
};

static int RunRef(string[] args)
{
    if (args.Length < 3)
    {
        Console.Error.WriteLine("用法: BmpRef ref <corpusRoot> <refOutDir>");
        return 2;
    }
    string corpusRoot = args[1];
    string refOutDir = args[2];
    if (!Directory.Exists(corpusRoot))
    {
        Console.Error.WriteLine($"# ERR corpusRoot 不存在: {corpusRoot}");
        return 2;
    }
    Directory.CreateDirectory(refOutDir);

    int rounds = 9;
    if (Environment.GetEnvironmentVariable("BMP_REF_ROUNDS") is { Length: > 0 } rs
        && int.TryParse(rs, NumberStyles.Integer, CultureInfo.InvariantCulture, out int rn) && rn > 0)
    {
        rounds = rn;
    }

    var files = CollectFiles(corpusRoot);

    // 全集暖机 1 遍：JIT/Tiered 提升在计时前全部收敛（对齐 jpeg/png 2026-11-08 公平化姿势。
    // TryDecodeIntoRgba8 自解析头，dest 复用尺寸正确的缓冲。
    foreach (var path in files)
    {
        try
        {
            byte[] wdata = File.ReadAllBytes(path);
            if (BmpImageDecoder.TryReadInfo(wdata, out ImageInfo wi))
            {
                _ = BmpImageDecoder.TryDecodeIntoRgba8(wdata, new byte[checked(wi.Width * wi.Height * 4)]);
            }
        }
        catch
        {
            // 暖机失败忽略
        }
    }

    int done = 0;
    foreach (var path in files)
    {
        string name = Path.GetFileName(path);
        string stem = Path.GetFileNameWithoutExtension(path);
        byte[] data;
        try
        {
            data = File.ReadAllBytes(path);
        }
        catch (Exception ex)
        {
            Console.WriteLine($"# ERR {name} 读文件失败: {ex.Message}");
            continue;
        }

        if (!BmpImageDecoder.TryReadInfo(data, out ImageInfo info))
        {
            Console.WriteLine($"# ERR {name} TryReadInfo 失败");
            continue;
        }
        int w = info.Width, h = info.Height;
        int len = checked(w * h * 4);
        var dest = new byte[len];

        if (!BmpImageDecoder.TryDecodeIntoRgba8(data, dest))
        {
            Console.WriteLine($"# ERR {name} TryDecodeIntoRgba8 失败");
            continue;
        }

        double ms = MedianMs(() =>
        {
            if (!BmpImageDecoder.TryDecodeIntoRgba8(data, dest))
            {
                throw new InvalidOperationException("decode");
            }
        }, rounds);

        try
        {
            File.WriteAllBytes(Path.Combine(refOutDir, $"{stem}.{w}x{h}.rgba"), dest);
        }
        catch (Exception ex)
        {
            Console.WriteLine($"# ERR {name} dump ref 失败: {ex.Message}");
            continue;
        }

        Console.WriteLine($"| `{name}` | {ms.ToString("F3", CultureInfo.InvariantCulture)} |");
        done++;
    }

    Console.WriteLine($"# DONE ref files={done} total={files.Count}");
    return 0;
}

static int RunGen(string[] args)
{
    if (args.Length < 3)
    {
        Console.Error.WriteLine("用法: BmpRef gen <corpusRoot> <outDir>");
        return 2;
    }
    string corpusRoot = args[1];
    string outDir = args[2];
    if (!Directory.Exists(corpusRoot))
    {
        Console.Error.WriteLine($"# ERR corpusRoot 不存在: {corpusRoot}");
        return 2;
    }

    string bmpDir = Path.Combine(corpusRoot, "BMP");
    string photoA = Path.Combine(bmpDir, "001.bmp");
    string photoB = Path.Combine(bmpDir, "002.bmp");
    if (!File.Exists(photoA) || !File.Exists(photoB))
    {
        Console.Error.WriteLine($"# ERR photo 源缺失: {photoA} / {photoB}");
        return 2;
    }

    // 覆盖式重生成：先清空 outDir（防旧语料残留混入）。
    if (Directory.Exists(outDir))
    {
        foreach (var f in Directory.EnumerateFiles(outDir, "*", SearchOption.AllDirectories))
            File.Delete(f);
    }
    else
    {
        Directory.CreateDirectory(outDir);
    }

    var (aw, ah, argb) = Decode(photoA);
    var (bw, bh, brgb) = Decode(photoB);
    Console.WriteLine($"# GEN photoA=001 {aw}x{ah} photoB=002 {bw}x{bh}");

    // 每个源各出 11 布局变体。
    int count = 0;
    count += EmitMatrix(outDir, "001", argb, aw, ah);
    count += EmitMatrix(outDir, "002", brgb, bw, bh);

    // 合成 screen 源（桌面 UI 风，确定性；带半透明阴影 → 真实 alpha）。
    int sw = 1920, sh = 1080;
    byte[] screen = SynthScreen(sw, sh);
    Console.WriteLine($"# GEN screen {sw}x{sh} (synth, rgba={screen.Length}B)");
    count += EmitMatrix(outDir, "screen", screen, sw, sh);

    // hand-rolled p2 / rgb565-444（singles 源，固定 4 色调色板便于精确 palette）。
    int singleW = 384, singleH = 288;
    byte[] singles = SynthSingles(singleW, singleH);
    count += EmitSingles(outDir, "singles", singles, singleW, singleH);

    Console.WriteLine($"# DONE gen files={count} outDir={outDir}");
    return 0;
}

// ---------------------------------------------------------------------------
// 布局矩阵（各源各出 1 份）
// ---------------------------------------------------------------------------

static int EmitMatrix(string outDir, string stem, byte[] rgba, int w, int h)
{
    var layouts = new (string Name, BmpEncodeOptions Opto)[]
    {
        ("rgb24", new BmpEncodeOptions { Format = BmpEncodeFormat.Rgb24 }),
        ("rgb24-topdown", new BmpEncodeOptions { Format = BmpEncodeFormat.Rgb24, TopDown = true }),
        // bgra32：Argb32 + Info 头 → 32bpp BI_RGB，第 4 字节为真实 alpha（kimg 丢 alpha 的已知分歧类）。
        ("bgra32", new BmpEncodeOptions { Format = BmpEncodeFormat.Argb32, Header = BmpHeaderVersion.Info }),
        // argb32-v4：Argb32 + V4 头 → BI_BITFIELDS，带 alpha 掩码（kimg BITFIELDS alpha → RGBA8）。
        ("argb32-v4", new BmpEncodeOptions { Format = BmpEncodeFormat.Argb32, Header = BmpHeaderVersion.V4 }),
        ("rgb555", new BmpEncodeOptions { Format = BmpEncodeFormat.Rgb555 }),
        ("rgb565", new BmpEncodeOptions { Format = BmpEncodeFormat.Rgb565 }),
        ("p8", new BmpEncodeOptions { Format = BmpEncodeFormat.Indexed8 }),
        ("p4", new BmpEncodeOptions { Format = BmpEncodeFormat.Indexed4 }),
        ("p1", new BmpEncodeOptions { Format = BmpEncodeFormat.Indexed1 }),
        ("rle8", new BmpEncodeOptions { Format = BmpEncodeFormat.Indexed8, Rle = true }),
        ("rle4", new BmpEncodeOptions { Format = BmpEncodeFormat.Indexed4, Rle = true }),
    };
    int count = 0;
    foreach (var (name, opt) in layouts)
    {
        byte[] file;
        try
        {
            file = BmpEncoder.Encode(rgba, w, h, PixelFormat.Rgba32, opt);
        }
        catch (Exception ex)
        {
            Console.WriteLine($"# ERR {stem}__{name} 编码失败: {ex.Message}");
            continue;
        }
        string outPath = Path.Combine(outDir, $"{stem}__{name}.bmp");
        File.WriteAllBytes(outPath, file);
        Console.WriteLine($"# GEN {stem}__{name}.bmp {w}x{h} {file.Length}B");
        count++;
    }
    return count;
}

// ---------------------------------------------------------------------------
// hand-rolled 低层写入（Sdcb 编码器产不出的布局）
// ---------------------------------------------------------------------------

static int EmitSingles(string outDir, string stem, byte[] rgba, int w, int h)
{
    // p2：2-bit 调色板（4 色），BI_RGB，逐行打包底→顶。
    {
        // 固定 4 色调色板（BGRX 项，与 singles 内容一致 → 精确 palette）。
        uint[] cols = [0xFFFFFFu, 0xC8C8C8u, 0x3C6EB4u, 0x14141Eu]; // 00RRGGBB
        var palette = new byte[4 * 4];
        for (var i = 0; i < 4; i++)
        {
            palette[i * 4] = (byte)cols[i];          // B
            palette[i * 4 + 1] = (byte)(cols[i] >> 8);  // G
            palette[i * 4 + 2] = (byte)(cols[i] >> 16); // R
            palette[i * 4 + 3] = 0;
        }
        int rowBytes = ((w * 2 + 31) / 32) * 4;
        var pixels = new byte[rowBytes * h];
        for (var y = 0; y < h; y++)
        {
            int srcY = h - 1 - y; // 底→顶
            int row = y * rowBytes;
            for (var x = 0; x < w; x++)
            {
                int r = rgba[srcY * w * 4 + 4 * x];
                int g = rgba[srcY * w * 4 + 4 * x + 1];
                int b = rgba[srcY * w * 4 + 4 * x + 2];
                int idx = Nearest4(r, g, b);
                int bitShift = 6 - 2 * (x & 3);
                pixels[row + x / 4] |= (byte)(idx << bitShift);
            }
        }
        byte[] file = WriteBmp(w, h, 2, 0, null, palette, pixels);
        File.WriteAllBytes(Path.Combine(outDir, $"{stem}__p2.bmp"), file);
        Console.WriteLine($"# GEN {stem}__p2.bmp {w}x{h} {file.Length}B (hand-rolled 2bpp)");
    }
    // rgb565-444：16bit BITFIELDS 4-4-4 掩码。
    {
        int rowBytes = (w * 2 + 3) & ~3;
        var pixels = new byte[rowBytes * h];
        for (var y = 0; y < h; y++)
        {
            int srcY = h - 1 - y; // 底→顶
            int row = y * rowBytes;
            for (var x = 0; x < w; x++)
            {
                int r = rgba[srcY * w * 4 + 4 * x];
                int g = rgba[srcY * w * 4 + 4 * x + 1];
                int b = rgba[srcY * w * 4 + 4 * x + 2];
                ushort v = (ushort)((r >> 4) << 8 | (g >> 4) << 4 | (b >> 4));
                BinaryPrimitives.WriteUInt16LittleEndian(pixels.AsSpan(row + 2 * x, 2), v);
            }
        }
        byte[] file = WriteBmp(w, h, 16, 3, (0x0F00u, 0x00F0u, 0x000Fu), [], pixels);
        File.WriteAllBytes(Path.Combine(outDir, $"{stem}__rgb565-444.bmp"), file);
        Console.WriteLine($"# GEN {stem}__rgb565-444.bmp {w}x{h} {file.Length}B (hand-rolled 444 bitfields)");
    }
    return 2;
}

/// <summary>到固定 4 色调色板（与 singles 内容固有 4 色一致）的最近索引：按 RGB 距离。</summary>
static int Nearest4(int r, int g, int b)
{
    int[] rs = [255, 200, 60, 20], gs = [255, 200, 110, 20], bs = [255, 200, 180, 30];
    int best = 0, bestD = int.MaxValue;
    for (var i = 0; i < 4; i++)
    {
        int dr = r - rs[i], dg = g - gs[i], db = b - bs[i];
        int d = dr * dr + dg * dg + db * db;
        if (d < bestD) { bestD = d; best = i; }
    }
    return best;
}

/// <summary>极简 BMP 写入器：14 文件头 + 40 Info 头（+ 可选 3×u32 掩码）+ 调色板 + 逐行像素（底→顶，行 4 字节对齐）。</summary>
static byte[] WriteBmp(int width, int height, int bpp, uint compression,
    (uint R, uint G, uint B)? masks, byte[] palette, byte[] pixels)
{
    int headerSize = 40;
    int maskBytes = masks is null ? 0 : 12;
    int palBytes = palette.Length;
    int pixelOffset = 14 + headerSize + maskBytes + palBytes;
    var o = new byte[pixelOffset + pixels.Length];
    o[0] = (byte)'B';
    o[1] = (byte)'M';
    BinaryPrimitives.WriteUInt32LittleEndian(o.AsSpan(2, 4), (uint)o.Length);
    BinaryPrimitives.WriteUInt32LittleEndian(o.AsSpan(10, 4), (uint)pixelOffset);
    BinaryPrimitives.WriteUInt32LittleEndian(o.AsSpan(14, 4), (uint)headerSize);
    BinaryPrimitives.WriteUInt32LittleEndian(o.AsSpan(18, 4), (uint)width);
    BinaryPrimitives.WriteUInt32LittleEndian(o.AsSpan(22, 4), (uint)height); // 正 = 底→顶
    BinaryPrimitives.WriteUInt16LittleEndian(o.AsSpan(26, 2), 1);
    BinaryPrimitives.WriteUInt16LittleEndian(o.AsSpan(28, 2), (ushort)bpp);
    BinaryPrimitives.WriteUInt32LittleEndian(o.AsSpan(30, 4), compression);
    BinaryPrimitives.WriteUInt32LittleEndian(o.AsSpan(34, 4), (uint)pixels.Length);
    if (masks is { } m)
    {
        BinaryPrimitives.WriteUInt32LittleEndian(o.AsSpan(54, 4), m.R);
        BinaryPrimitives.WriteUInt32LittleEndian(o.AsSpan(58, 4), m.G);
        BinaryPrimitives.WriteUInt32LittleEndian(o.AsSpan(62, 4), m.B);
    }
    if (palBytes > 0) palette.CopyTo(o, 54 + maskBytes);
    pixels.CopyTo(o, pixelOffset);
    return o;
}

// ---------------------------------------------------------------------------
// 合成源
// ---------------------------------------------------------------------------

/// <summary>确定性桌面 UI 风合成图（窗口块/标题栏/阴影/字形/渐变/任务栏/进度条），带半透明阴影 = 真实 alpha。</summary>
static byte[] SynthScreen(int w, int h)
{
    var px = new byte[w * h * 4];
    // 垂直渐变背景（浅灰 → 蓝灰）。
    for (var y = 0; y < h; y++)
    {
        int v = 52 + y * 28 / h;
        for (var x = 0; x < w; x++)
        {
            int u = v + (x * 6) / w;
            SetPx(px, w, x, y, (byte)u, (byte)v, (byte)(v + 8), 255);
        }
    }
    // 任务栏（底部深色条，含高亮线）。
    FillRect(px, w, h, 0, h - 48, w, h, 28, 30, 40, 255);
    FillRect(px, w, h, 0, h - 49, w, h - 48, 90, 140, 210, 255);

    // 主窗口（左）。
    int x0 = 60, y0 = 70, winW = 980, winH = 620;
    Shadow(px, w, h, x0 + 10, y0 + 14, winW, winH, 10, 0, 0, 0, 70);   // 右下软阴影
    FillRect(px, w, h, x0, y0, x0 + winW, y0 + 34, 56, 96, 152, 255);  // 标题栏
    FillRect(px, w, h, x0, y0 + 34, x0 + winW, y0 + winH, 246, 248, 250, 255);
    for (var i = 0; i < 9; i++) // 标题字形行
        FillRect(px, w, h, x0 + 36 + i * 34, y0 + 10, x0 + 66 + i * 34, y0 + 24, 232, 236, 240, 255);
    // 窗口内文字区（行块）。
    for (var i = 0; i < 12; i++)
    {
        int yy = y0 + 62 + i * 40;
        FillRect(px, w, h, x0 + 28, yy, x0 + 28 + 220 + (i % 3) * 90, yy + 14, 150, 158, 168, 255);
    }
    // 左侧导航 + 选中高亮。
    FillRect(px, w, h, x0, y0 + 34, x0 + 200, y0 + winH, 236, 240, 244, 255);
    FillRect(px, w, h, x0 + 8, y0 + 50, x0 + 192, y0 + 78, 96, 150, 216, 255);
    FillRect(px, w, h, x0 + 8, y0 + 92, x0 + 192, y0 + 116, 210, 216, 224, 255);
    FillRect(px, w, h, x0 + 8, y0 + 130, x0 + 192, y0 + 154, 210, 216, 224, 255);

    // 副窗口（右上，较小）。
    int bx0 = 1110, by0 = 120, bw2 = 640, bh2 = 360;
    Shadow(px, w, h, bx0 + 8, by0 + 10, bw2, bh2, 8, 0, 0, 0, 60);
    FillRect(px, w, h, bx0, by0, bx0 + bw2, by0 + 30, 120, 110, 108, 255); // 深灰标题栏
    FillRect(px, w, h, bx0, by0 + 30, bx0 + bw2, by0 + bh2, 252, 252, 250, 255);
    for (var i = 0; i < 9; i++) // 图表条（渐变蓝）
        FillRect(px, w, h, bx0 + 30 + i * 60, by0 + bh2 - 60 - (i * 34 % 150), bx0 + 74 + i * 60, by0 + bh2 - 46, 66, 132, 210, 255);

    // 对话框：带半透明（alpha 90）面板，产生真实 alpha（bgra32/argb32-v4 的 alpha 分歧来源）。
    int dx0 = 240, dy0 = 560, dw = 460, dh = 210;
    Shadow(px, w, h, dx0 + 8, dy0 + 10, dw, dh, 9, 0, 0, 0, 60);
    FillRect(px, w, h, dx0, dy0, dx0 + dw, dy0 + dh, 60, 90, 140, 90); // 半透明蓝面板
    FillRect(px, w, h, dx0 + 12, dy0 + 12, dx0 + dw - 12, dy0 + dh - 12, 250, 250, 250, 96);
    for (var i = 0; i < 3; i++) // 按钮
        FillRect(px, w, h, dx0 + 40 + i * 130, dy0 + dh - 54, dx0 + 150 + i * 130, dy0 + dh - 22, 40, 96, 170, 120);

    // 进度条（底部区域上方横条）。
    FillRect(px, w, h, 80, h - 96, w - 80, h - 82, 210, 216, 224, 255);
    FillRect(px, w, h, 84, h - 92, 84 + (w - 168) * 3 / 5, h - 86, 72, 160, 96, 255);
    return px;
}

/// <summary>固定 4 色的 singles 源（仅供 p2 / 444 hand-rolled 布局）。</summary>
static byte[] SynthSingles(int w, int h)
{
    var px = new byte[w * h * 4];
    FillRect(px, w, h, 0, 0, w, h, 255, 255, 255, 255);        // 白底
    FillRect(px, w, h, 24, 24, w - 24, h - 24, 200, 200, 200, 255); // 灰面板
    FillRect(px, w, h, w / 2 - 4, 24, w / 2 + 4, h - 24, 60, 110, 180, 255); // 蓝分隔条
    for (var i = 0; i < 5; i++)
        for (var j = 0; j < 3; j++)
            FillRect(px, w, h, 40 + i * 70, 60 + j * 70, 92 + i * 70, 88 + j * 70, 20, 20, 30, 255); // 深色字块
    return px;
}

static void SetPx(byte[] px, int w, int x, int y, byte r, byte g, byte b, byte a)
{
    int o = y * w * 4 + x * 4;
    px[o] = r;
    px[o + 1] = g;
    px[o + 2] = b;
    px[o + 3] = a;
}

static void FillRect(byte[] px, int w, int h, int x0, int y0, int x1, int y1, byte r, byte g, byte b, byte a)
{
    for (var y = Math.Max(0, y0); y < Math.Min(h, y1); y++)
        for (var x = Math.Max(0, x0); x < Math.Min(w, x1); x++)
            SetPx(px, w, x, y, r, g, b, a);
}

/// <summary>软阴影：右下偏移的暗色差边方块（alpha 向外衰减形成渐变透明边缘）。</summary>
static void Shadow(byte[] px, int w, int h, int x0, int y0, int winW, int winH, int size, byte r, byte g, byte b, byte a0)
{
    for (var y = y0; y < y0 + size; y++)
        for (var x = x0; x < x0 + winW + size; x++)
        {
            int inH = y - y0, inW = x - x0;
            int dist = Math.Max(Math.Max(inH, inW - winW), 0);
            int a = a0 * (size - Math.Min(size, dist)) / size;
            if (a <= 0 || y >= h || x >= w) continue;
            int o = y * w * 4 + x * 4;
            // 与背景混合（源上不透明背景已画好，这里半透明置入 → 最终 alpha 由 C# 编码保留）。
            // 为确定性：直接写出半透明黑（不预乘）。解码 RGBA 直读。
            px[o] = (byte)((px[o] * (255 - a) + r * a) / 255);
            px[o + 1] = (byte)((px[o + 1] * (255 - a) + g * a) / 255);
            px[o + 2] = (byte)((px[o + 2] * (255 - a) + b * a) / 255);
            px[o + 3] = a > px[o + 3] ? (byte)a : px[o + 3]; // 阴影 alpha 提升（从不透明边界向内渐隐）
        }
}

static (int W, int H, byte[] Rgba) Decode(string path)
{
    byte[] data;
    try
    {
        data = File.ReadAllBytes(path);
    }
    catch (Exception ex)
    {
        throw new InvalidDataException($"读文件失败 {path}: {ex.Message}");
    }
    if (!BmpImageDecoder.TryReadInfo(data, out ImageInfo info))
        throw new InvalidDataException($"TryReadInfo 失败 {path}");
    int w = info.Width, h = info.Height;
    var dest = new byte[checked(w * h * 4)];
    if (!BmpImageDecoder.TryDecodeIntoRgba8(data, dest))
        throw new InvalidDataException($"TryDecodeIntoRgba8 失败 {path}");
    return (w, h, dest);
}

/// <summary>语料 = <paramref name="corpusRoot"/>/BMP + <paramref name="corpusRoot"/>/BMP-MATRIX 下全部 *.bmp。</summary>
static List<string> CollectFiles(string corpusRoot)
{
    var out_ = new List<string>();
    foreach (var dir in new[] { "BMP", "BMP-MATRIX" })
    {
        string d = Path.Combine(corpusRoot, dir);
        if (Directory.Exists(d))
            out_.AddRange(Directory.EnumerateFiles(d, "*.bmp", SearchOption.TopDirectoryOnly));
    }
    return out_.OrderBy(f => f, StringComparer.Ordinal).ToList();
}

static int UsageReturn()
{
    Console.Error.WriteLine("用法: BmpRef <ref|gen> ...");
    return 2;
}

static void Usage()
{
    Console.Error.WriteLine("用法: BmpRef <ref|gen> ...");
}

static double MedianMs(Action action, int rounds, int warm = 2)
{
    for (int i = 0; i < warm; i++) { action(); } // 预热 warm 轮（不计时，丢弃）
    var acc = new double[rounds];
    for (int i = 0; i < rounds; i++)
    {
        long t0 = Stopwatch.GetTimestamp();
        action();
        long t1 = Stopwatch.GetTimestamp();
        acc[i] = (t1 - t0) * 1000.0 / Stopwatch.Frequency;
    }
    Array.Sort(acc);
    return acc.Length % 2 == 1 ? acc[acc.Length / 2] : (acc[acc.Length / 2 - 1] + acc[acc.Length / 2]) / 2.0;
}
