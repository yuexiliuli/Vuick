// GIF C# runner（单子命令）。
//
// `ref <corpusRoot> <refOutDir>` —— 单核参照测量：对 <corpusRoot>/GIF 下每个 *.gif 用
// Sdcb.SimdCodecs.Gif（GifImageDecoder.TryOpenFrames → 按序 TryDecodeFrameInto(..., Rgba32)）
// 计时**全帧解码总墙钟**（全集暖机 1 遍 + 每文件 warmup2 + GIF_REF_ROUNDS=9 中位），并把该文件
// **全部帧 RGBA8 拼接**（帧数 × w*h*4，静态=1 帧）dump 到
// <refOut>/<stem>.<w>x<h>.rgba（gif_csharp_parity 的字节恒等参照）。
// 表行 `| `name` | ms |`（全帧总耗时，供 compare_refs.py 消费）；每文件另打 `# <name> ... frames=N` 行。
//
// `gen <srcDir> <outDir>` —— 语料生成：源 = <srcDir>/001.bmp、002.bmp（Sdcb.SimdCodecs.Bmp 解 RGBA，
// 静态缩到 ≤1024 边、动画缩到 320×240）+ 内建合成 screen（1920×1080 低色数 UI 风）；
// GifEncoder/EncodeAnimation 产 **静态**（每源 × opaque/transparent × exact/wq，Dither=false/true）
// + **动画**（001/screen × opaque/transparent × loop{0,3}，801 8 帧、每帧 140ms），全部落 <outDir>
// （先清空防混）。C# 编码器只 89a、全局调色板、不写隔行（覆盖面缺口见 README）。

using System.Diagnostics;
using System.Globalization;
using Sdcb.SimdCodecs.Abstractions;
using Sdcb.SimdCodecs.Bmp;
using Sdcb.SimdCodecs.Gif;

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
        Console.Error.WriteLine("用法: GifRef ref <corpusRoot> <refOutDir>");
        return 2;
    }
    string corpusRoot = args[1];
    string refOutDir = args[2];
    string gifDir = Path.Combine(corpusRoot, "GIF");
    if (!Directory.Exists(gifDir))
    {
        Console.Error.WriteLine($"# ERR corpus GIF 不存在: {gifDir}");
        return 2;
    }
    Directory.CreateDirectory(refOutDir);

    int rounds = 9;
    if (Environment.GetEnvironmentVariable("GIF_REF_ROUNDS") is { Length: > 0 } rs
        && int.TryParse(rs, NumberStyles.Integer, CultureInfo.InvariantCulture, out int rn) && rn > 0)
    {
        rounds = rn;
    }

    var files = Directory.EnumerateFiles(gifDir, "*.gif", SearchOption.TopDirectoryOnly)
        .OrderBy(f => f, StringComparer.Ordinal)
        .ToList();

    // 全集暖机 1 遍：JIT/Tiered 提升在计时前全部收敛（对齐 tiff 2026-11-08 公平化姿势）。
    foreach (var path in files)
    {
        try
        {
            byte[] wdata = File.ReadAllBytes(path);
            if (GifImageDecoder.TryReadInfo(wdata, out _) && GifImageDecoder.TryOpenFrames(wdata, out IImageFrameReader? wreader) && wreader is not null)
            {
                using (wreader)
                {
                    var dest = new byte[64];
                    int w = 0, h = 0;
                    if (wreader.TryGetFrameInfo(0, out var fi)) { w = fi.Info.Width; h = fi.Info.Height; }
                    if (w > 0 && h > 0) dest = new byte[checked(w * h * 4)];
                    for (int i = 0; i < wreader.FrameCount; i++)
                        _ = wreader.TryDecodeFrameInto(i, dest, 0, PixelFormat.Rgba32);
                }
            }
        }
        catch { /* 暖机失败忽略 */ }
    }

    int done = 0, totalFrames = 0;
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

        if (!GifImageDecoder.TryReadInfo(data, out ImageInfo info))
        {
            Console.WriteLine($"# ERR {name} TryReadInfo 失败");
            continue;
        }
        int w = info.Width, h = info.Height;
        int frameBytes = checked(w * h * 4);
        var dest = new byte[frameBytes];

        if (!GifImageDecoder.TryOpenFrames(data, out IImageFrameReader? reader) || reader is null)
        {
            Console.WriteLine($"# ERR {name} TryOpenFrames 失败");
            continue;
        }
        using (reader)
        {
            int frames = reader.FrameCount;

            double ms = MedianMs(() =>
            {
                for (int i = 0; i < frames; i++)
                {
                    if (!reader.TryDecodeFrameInto(i, dest, 0, PixelFormat.Rgba32))
                        throw new InvalidOperationException($"帧 {i} 解码失败");
                }
            }, rounds);

            // 全帧 RGBA8 拼接 dump（静态=1 帧）：gif_csharp_parity 的字节恒等参照。
            var outBuf = new byte[checked(frameBytes * frames)];
            for (int i = 0; i < frames; i++)
            {
                if (!reader.TryDecodeFrameInto(i, dest, 0, PixelFormat.Rgba32))
                {
                    Console.WriteLine($"# ERR {name} 帧 {i} dump 解码失败");
                    break;
                }
                Buffer.BlockCopy(dest, 0, outBuf, i * frameBytes, frameBytes);
            }
            try
            {
                File.WriteAllBytes(Path.Combine(refOutDir, $"{stem}.{w}x{h}.rgba"), outBuf);
            }
            catch (Exception ex)
            {
                Console.WriteLine($"# ERR {name} dump ref 失败: {ex.Message}");
                continue;
            }

            Console.WriteLine($"# {name} {w}x{h} frames={frames} dump_bytes={outBuf.Length} ms={ms.ToString("F3", CultureInfo.InvariantCulture)}");
            Console.WriteLine($"| `{name}` | {ms.ToString("F3", CultureInfo.InvariantCulture)} |");
            done++;
            totalFrames += frames;
        }
    }

    Console.WriteLine($"# DONE ref files={done} total={files.Count} total_frames={totalFrames}");
    return 0;
}

// ---------------------------------------------------------------------------
// 语料生成
// ---------------------------------------------------------------------------

static int RunGen(string[] args)
{
    if (args.Length < 3)
    {
        Console.Error.WriteLine("用法: GifRef gen <srcDir> <outDir>");
        return 2;
    }
    string srcDir = args[1];
    string outDir = args[2];
    if (!Directory.Exists(srcDir))
    {
        Console.Error.WriteLine($"# ERR srcDir 不存在: {srcDir}");
        return 2;
    }
    Directory.CreateDirectory(outDir);
    // 先清空防混（只删 *.gif）。
    foreach (var f in Directory.EnumerateFiles(outDir, "*.gif"))
        File.Delete(f);

    var screen = BuildScreen(); // 1920×1080 低色数 UI 风
    GenStaticSet(Path.Combine(outDir, "screen"), screen, 1920, 1080);
    GenAnimSet(Path.Combine(outDir, "screen"), Bilinear(screen, 1920, 1080, 320, 240), 320, 240);

    foreach (var src in new[] { "001", "002" })
    {
        byte[]? rgba = ReadBmpRgba(Path.Combine(srcDir, $"{src}.bmp"), out int sw, out int sh);
        if (rgba is null)
        {
            Console.WriteLine($"# ERR {src}.bmp 解码失败，跳过该源");
            continue;
        }
        (int sw2, int sh2) = FitScale(sw, sh, 1024);
        GenStaticSet(Path.Combine(outDir, src), Bilinear(rgba, sw, sh, sw2, sh2), sw2, sh2);
        if (src == "001")
            GenAnimSet(Path.Combine(outDir, src), Bilinear(rgba, sw, sh, 320, 240), 320, 240);
    }

    Console.WriteLine("# DONE gen");
    return 0;
}

static void GenStaticSet(string baseName, byte[] rgba, int w, int h)
{
    // 透明变体：确定性打孔 ~1/7 像素 alpha=0（<128 透明）。
    var trans = (byte[])rgba.Clone();
    for (int i = 0; i < w * h; i++)
    {
        if (((i * 7919 + (i / w) * 104729) & 0x7) == 0)
            trans[i * 4 + 3] = 0;
    }
    WriteGif($"{baseName}__static_opaque_exact.gif", rgba, w, h, new GifEncodeOptions { Dither = false });
    WriteGif($"{baseName}__static_opaque_wq.gif", rgba, w, h, new GifEncodeOptions { Dither = true });
    WriteGif($"{baseName}__static_transparent_exact.gif", trans, w, h, new GifEncodeOptions { Dither = false });
    WriteGif($"{baseName}__static_transparent_wq.gif", trans, w, h, new GifEncodeOptions { Dither = true });
}

static void GenAnimSet(string baseName, byte[] rgba, int w, int h)
{
    var opaque = BuildAnimFrames(rgba, w, h, transparent: false);
    var transp = BuildAnimFrames(rgba, w, h, transparent: true);
    foreach (var loop in new[] { 0, 3 })
    {
        WriteGifAnim($"{baseName}__anim_opaque_loop{loop}.gif", opaque, w, h, new GifEncodeOptions { LoopCount = loop, Dither = false });
        WriteGifAnim($"{baseName}__anim_transparent_loop{loop}.gif", transp, w, h, new GifEncodeOptions { LoopCount = loop, Dither = false });
    }
}

/// 8 帧动画：逐帧水平滚动（循环位移）；transparent 时再加一个移动的 alpha=0 圆孔（16px 半径）。
static List<GifAnimationFrame> BuildAnimFrames(byte[] src, int w, int h, bool transparent)
{
    const int frames = 8;
    int shiftStep = Math.Max(1, w / frames);
    var cur = new byte[w * h * 4];
    var list = new List<GifAnimationFrame>(frames);
    for (int f = 0; f < frames; f++)
    {
        int shift = f * shiftStep;
        for (int y = 0; y < h; y++)
        {
            int baseRow = y * w * 4;
            for (int x = 0; x < w; x++)
            {
                int sx = (x + shift) % w;
                Array.Copy(src, baseRow + sx * 4, cur, baseRow + x * 4, 4);
            }
        }
        if (transparent)
        {
            int cx = w / 2 + f * (w / 2 - 24) / (frames - 1);
            int cy = h / 2 + (int)(Math.Sin(f * 1.4) * (h / 4));
            for (int y = Math.Max(0, cy - 16); y < Math.Min(h, cy + 16); y++)
            {
                for (int x = Math.Max(0, cx - 16); x < Math.Min(w, cx + 16); x++)
                {
                    int dx = x - cx, dy = y - cy;
                    if (dx * dx + dy * dy <= 16 * 16)
                        cur[(y * w + x) * 4 + 3] = 0;
                }
            }
        }
        list.Add(new GifAnimationFrame(new ReadOnlyMemory<byte>((byte[])cur.Clone()), TimeSpan.FromMilliseconds(140)));
    }
    return list;
}

static void WriteGif(string path, byte[] rgba, int w, int h, GifEncodeOptions opts)
{
    byte[] gif = GifEncoder.Encode(rgba, w, h, 4, opts);
    File.WriteAllBytes(path, gif);
    Console.WriteLine($"# gen {Path.GetFileName(path)} {w}x{h} anim=no frames=1 bytes={gif.Length}");
}

static void WriteGifAnim(string path, List<GifAnimationFrame> frames, int w, int h, GifEncodeOptions opts)
{
    byte[] gif = GifEncoder.EncodeAnimation(frames, w, h, 4, opts);
    File.WriteAllBytes(path, gif);
    Console.WriteLine($"# gen {Path.GetFileName(path)} {w}x{h} anim=yes frames={frames.Count} bytes={gif.Length}");
}

/// 合成 screen：1920×1080 低色数 UI 风（色块/分级渐变/条纹/字符，恒 ≤255 不透明色 → exact 无损）。
static byte[] BuildScreen()
{
    const int w = 1920, h = 1080;
    var px = new byte[w * h * 4];
    void Fill(int x0, int y0, int x1, int y1, int r, int g, int b)
    {
        for (int y = y0; y < y1; y++)
            for (int x = x0; x < x1; x++)
            {
                int o = (y * w + x) * 4;
                px[o] = (byte)r; px[o + 1] = (byte)g; px[o + 2] = (byte)b; px[o + 3] = 255;
            }
    }
    // 底色与面板。
    Fill(0, 0, w, h, 0x20, 0x24, 0x2a);
    Fill(0, 0, w, 120, 0x3a, 0x42, 0x50);          // 顶栏
    Fill(0, 120, 320, h, 0x2f, 0x35, 0x42);        // 左侧栏
    Fill(320, 120, w, 1080, 0x27, 0x2b, 0x33);     // 内容区
    // 内容区色块。
    Fill(360, 160, 760, 460, 0xe7, 0x4c, 0x3c);    // 红
    Fill(800, 160, 1200, 460, 0x34, 0x98, 0xdb);   // 蓝
    Fill(1240, 160, 1640, 460, 0x2e, 0xcc, 0x71);  // 绿
    Fill(360, 500, 1640, 700, 0xf1, 0xc4, 0x0f);   // 黄
    // 分级渐变（8 级，保持 ≤255 色数）。
    for (int y = 740; y < 900; y++)
        for (int x = 360; x < 1640; x++)
        {
            int step = ((x - 360) * 8) / (1640 - 360);
            (int rr, int gg, int bb) = step switch
            {
                0 => (0x00, 0x00, 0x80),
                1 => (0x00, 0x00, 0xff),
                2 => (0x00, 0x80, 0xff),
                3 => (0x00, 0xff, 0xff),
                4 => (0x00, 0xff, 0x00),
                5 => (0xff, 0xff, 0x00),
                6 => (0xff, 0x80, 0x00),
                _ => (0xff, 0x00, 0x00),
            };
            Fill(x, y, x + 1, y + 1, rr, gg, bb);
        }
    // 条纹区（2 色）。
    for (int y = 940; y < 1040; y++)
        for (int x = 360; x < 1640; x++)
            Fill(x, y, x + 1, y + 1, ((x / 8) % 2) == 0 ? 0xff : 0x10, ((x / 8) % 2) == 0 ? 0xff : 0xf0, ((x / 8) % 2) == 0 ? 0xff : 0x10);
    // 顶栏字符 "GIF"（5×7 点阵，放大 8 倍）。
    string[] font = FontGlyph('G');
    DrawText(px, w, 60, 32, 8, font);
    font = FontGlyph('I');
    DrawText(px, w, 60 + 8 * 5 + 24, 32, 8, font);
    font = FontGlyph('F');
    DrawText(px, w, 60 + 2 * (8 * 5 + 24), 32, 8, font);
    return px;
}

// 5×7 点阵（0=空 1=前景 0xf1 0xc4 0x0f）。
static string[] FontGlyph(char c) => c switch
{
    'G' => new[] { "01110", "10001", "10000", "10111", "10001", "10001", "01110" },
    'I' => new[] { "11111", "00100", "00100", "00100", "00100", "00100", "11111" },
    'F' => new[] { "11111", "10000", "10000", "11110", "10000", "10000", "10000" },
    _ => new[] { "00000", "00000", "00000", "00000", "00000", "00000", "00000" },
};

static void DrawText(byte[] px, int w, int x0, int y0, int scale, string[] glyph)
{
    for (int ry = 0; ry < 7; ry++)
        for (int rx = 0; rx < 5; rx++)
        {
            if (glyph[ry][rx] != '1') continue;
            for (int dy = 0; dy < scale; dy++)
                for (int dx = 0; dx < scale; dx++)
                {
                    int x = x0 + rx * scale + dx, y = y0 + ry * scale + dy;
                    if (x < 0 || x >= w || y < 0) continue;
                    int o = (y * w + x) * 4;
                    px[o] = 0xf1; px[o + 1] = 0xc4; px[o + 2] = 0x0f; px[o + 3] = 255;
                }
        }
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

static byte[]? ReadBmpRgba(string path, out int w, out int h)
{
    w = 0; h = 0;
    try
    {
        byte[] data = File.ReadAllBytes(path);
        if (!BmpImageDecoder.TryReadInfo(data, out ImageInfo fi)) { Console.WriteLine($"# ERR {path} BMP TryReadInfo 失败"); return null; }
        w = fi.Width; h = fi.Height;
        var buf = new byte[checked(w * h * 4)];
        if (!BmpImageDecoder.TryDecodeIntoRgba8(data, buf)) { Console.WriteLine($"# ERR {path} BMP TryDecodeIntoRgba8 失败"); return null; }
        return buf;
    }
    catch (Exception ex)
    {
        Console.WriteLine($"# ERR {path} BMP 解码异常: {ex.Message}");
        return null;
    }
}

static (int w, int h) FitScale(int sw, int sh, int maxEdge)
{
    double s = Math.Min(1.0, (double)maxEdge / Math.Max(sw, sh));
    return (Math.Max(1, (int)Math.Round(sw * s)), Math.Max(1, (int)Math.Round(sh * s)));
}

/// 双线性缩放到目标尺寸（确定性；吃透全部通道，含 alpha）。
static byte[] Bilinear(byte[] src, int sw, int sh, int dw, int dh)
{
    var dst = new byte[dw * dh * 4];
    for (int y = 0; y < dh; y++)
    {
        float fy = (y + 0.5f) * sh / dh - 0.5f;
        int y0 = Math.Clamp((int)MathF.Floor(fy), 0, sh - 1);
        int y1 = Math.Clamp(y0 + 1, 0, sh - 1);
        float wy = fy - y0;
        for (int x = 0; x < dw; x++)
        {
            float fx = (x + 0.5f) * sw / dw - 0.5f;
            int x0 = Math.Clamp((int)MathF.Floor(fx), 0, sw - 1);
            int x1 = Math.Clamp(x0 + 1, 0, sw - 1);
            float wx = fx - x0;
            int o = (y * dw + x) * 4;
            for (int c = 0; c < 4; c++)
            {
                float v =
                    (1 - wy) * ((1 - wx) * src[(y0 * sw + x0) * 4 + c] + wx * src[(y0 * sw + x1) * 4 + c]) +
                    wy * ((1 - wx) * src[(y1 * sw + x0) * 4 + c] + wx * src[(y1 * sw + x1) * 4 + c]);
                dst[o + c] = (byte)Math.Clamp((int)MathF.Round(v), 0, 255);
            }
        }
    }
    return dst;
}

static int UsageReturn()
{
    Console.Error.WriteLine("用法: GifRef <ref|gen> ...");
    return 2;
}

static void Usage()
{
    Console.Error.WriteLine("用法: GifRef <ref|gen> ...");
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
