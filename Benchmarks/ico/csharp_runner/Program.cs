// ICO C# runner（单子命令）。
//
// `ref <corpusRoot> <refOutDir>` —— 单核参照测量：对 <corpusRoot>/ICO 下每个 *.ico + *.cur 用
// Sdcb.SimdCodecs.Ico 的 `IcoDecoder.Decode(data)`（读目录 → Best 条目 → 解码成 RGBA8，icon 与
// cursor 同路径）计时（全集暖机 1 遍 + 每文件 warmup2 + ICO_REF_ROUNDS=9 中位），并把该文件 **Best
// 条目** RGBA8（w*h*4）dump 到 <refOut>/<stem>.<w>x<h>.rgba（ico_csharp_parity 的字节恒等参照）。
// 表行 `| `name` | ms |`（供 compare_refs.py 消费）；每文件另打 `# <name> <w>x<h> ...` 行。
//
// `gen <srcDir> <outDir>` —— 语料生成：源 = <srcDir>/001.bmp、002.bmp（Sdcb.SimdCodecs.Bmp 解 RGBA）
// + 内建合成 screen（1920×1080 低色数 UI 风）；双线性缩放到条目尺寸后 IcoEncoder/EncodeCursor 生成：
//   单尺寸容器：{16,32,48} × {Png, Bmp32} × {opaque, transparent}（alpha 仅 {0,255}；transparent 为
//              确定性打孔 ~1/7 像素 alpha=0）——每源 12 文件 `<src>__<size>_<type>_[opq|trp].ico`
//   多尺寸容器：Auto（16,32,48,256 四条目，全 32bpp 深度）——每源 1 文件 `<src>__multi.ico`
//   CUR（热点）：32×32 透明光标（Auto → BMP32）——2 文件 `<src>__cur.ico`（001 与 screen）
// 全部落 <outDir>（**先清空 *.ico/*.cur 防混**）。**不含**混合位深容器与中间 alpha（SKIP 面，见 README）。

using System.Diagnostics;
using System.Globalization;
using Sdcb.SimdCodecs.Abstractions;
using Sdcb.SimdCodecs.Bmp;
using Sdcb.SimdCodecs.Ico;

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
        Console.Error.WriteLine("用法: IcoRef ref <corpusRoot> <refOutDir>");
        return 2;
    }
    string corpusRoot = args[1];
    string refOutDir = args[2];
    string icoDir = Path.Combine(corpusRoot, "ICO");
    if (!Directory.Exists(icoDir))
    {
        Console.Error.WriteLine($"# ERR corpus ICO 不存在: {icoDir}");
        return 2;
    }
    Directory.CreateDirectory(refOutDir);

    int rounds = 9;
    if (Environment.GetEnvironmentVariable("ICO_REF_ROUNDS") is { Length: > 0 } rs
        && int.TryParse(rs, NumberStyles.Integer, CultureInfo.InvariantCulture, out int rn) && rn > 0)
    {
        rounds = rn;
    }

    var files = Directory.EnumerateFiles(icoDir)
        .Where(f => f.EndsWith(".ico", StringComparison.OrdinalIgnoreCase) || f.EndsWith(".cur", StringComparison.OrdinalIgnoreCase))
        .OrderBy(f => f, StringComparer.Ordinal)
        .ToList();

    // 全集暖机 1 遍：JIT/Tiered 提升在计时前全部收敛（对齐 gif/tiff 2026-11-08 公平化姿势）。
    foreach (var path in files)
    {
        try
        {
            byte[] wdata = File.ReadAllBytes(path);
            _ = IcoDecoder.Decode(wdata);
        }
        catch { /* 暖机失败忽略 */ }
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

        IcoImage img;
        try
        {
            img = IcoDecoder.Decode(data);
        }
        catch (Exception ex)
        {
            Console.WriteLine($"# ERR {name} Decode 失败: {ex.Message}");
            continue;
        }
        int w = img.Width, h = img.Height;
        var rgba = img.Rgba;

        double ms = MedianMs(() =>
        {
            _ = IcoDecoder.Decode(data);
        }, rounds);

        try
        {
            File.WriteAllBytes(Path.Combine(refOutDir, $"{stem}.{w}x{h}.rgba"), rgba);
        }
        catch (Exception ex)
        {
            Console.WriteLine($"# ERR {name} dump ref 失败: {ex.Message}");
            continue;
        }

        Console.WriteLine($"# {name} {w}x{h} dump_bytes={rgba.Length} ms={ms.ToString("F3", CultureInfo.InvariantCulture)}");
        Console.WriteLine($"| `{name}` | {ms.ToString("F3", CultureInfo.InvariantCulture)} |");
        done++;
    }

    Console.WriteLine($"# DONE ref files={done} total={files.Count}");
    return 0;
}

// ---------------------------------------------------------------------------
// 语料生成
// ---------------------------------------------------------------------------

static int RunGen(string[] args)
{
    if (args.Length < 3)
    {
        Console.Error.WriteLine("用法: IcoRef gen <srcDir> <outDir>");
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
    // 先清空防混（只删 *.ico/*.cur）。
    foreach (var f in Directory.EnumerateFiles(outDir)
        .Where(f => f.EndsWith(".ico", StringComparison.OrdinalIgnoreCase) || f.EndsWith(".cur", StringComparison.OrdinalIgnoreCase)))
    {
        File.Delete(f);
    }

    var screen = BuildScreen(); // 1920×1080 低色数 UI 风
    GenSourceSet(outDir, "screen", screen, 1920, 1080, cur: true);

    foreach (var src in new[] { "001", "002" })
    {
        byte[]? rgba = ReadBmpRgba(Path.Combine(srcDir, $"{src}.bmp"), out int sw, out int sh);
        if (rgba is null)
        {
            Console.WriteLine($"# ERR {src}.bmp 解码失败，跳过该源");
            continue;
        }
        GenSourceSet(outDir, src, rgba, sw, sh, cur: src == "001");
    }

    Console.WriteLine("# DONE gen");
    return 0;
}

/// 每源语料：单尺寸 {16,32,48}×{png,bmp32}×{opq,trp} + multi(16,32,48,256 Auto) + 可选 cur(32×32)。
static void GenSourceSet(string outDir, string src, byte[] rgba, int sw, int sh, bool cur)
{
    foreach (var size in new[] { 16, 32, 48 })
    {
        var s = Bilinear(rgba, sw, sh, size, size);
        var trp = MakeTransparent(s, size, size);
        WriteIco(Path.Combine(outDir, $"{src}__{size}_png_opq.ico"), s, size, size, IcoImageEncoding.Png);
        WriteIco(Path.Combine(outDir, $"{src}__{size}_png_trp.ico"), trp, size, size, IcoImageEncoding.Png);
        WriteIco(Path.Combine(outDir, $"{src}__{size}_bmp32_opq.ico"), s, size, size, IcoImageEncoding.Bmp32);
        WriteIco(Path.Combine(outDir, $"{src}__{size}_bmp32_trp.ico"), trp, size, size, IcoImageEncoding.Bmp32);
    }

    // 多尺寸容器：Auto（<256 → BMP32，>=256 → PNG），全 32bpp 深度 → Best（面积）一致。
    var frames = new List<IcoFrame>();
    foreach (var size in new[] { 16, 32, 48, 256 })
    {
        var s = Bilinear(rgba, sw, sh, size, size);
        frames.Add(new IcoFrame(s, size, size, PixelFormat.Rgba32, IcoImageEncoding.Auto));
    }
    byte[] multi = IcoEncoder.Encode(frames);
    File.WriteAllBytes(Path.Combine(outDir, $"{src}__multi.ico"), multi);
    Console.WriteLine($"# gen {src}__multi.ico entries=4 bytes={multi.Length}");

    if (cur)
    {
        // CUR 32×32 光标（透明 → Auto=BMP32 + AND 掩码），热点 (5,5)。
        var s = Bilinear(rgba, sw, sh, 32, 32);
        var trp = MakeTransparent(s, 32, 32);
        var curFrames = new[] { new IcoFrame(trp, 32, 32, PixelFormat.Rgba32, IcoImageEncoding.Auto, 5, 5) };
        byte[] curBytes = IcoEncoder.EncodeCursor(curFrames);
        File.WriteAllBytes(Path.Combine(outDir, $"{src}__cur.ico"), curBytes);
        Console.WriteLine($"# gen {src}__cur.ico 32x32 hotspot=(5,5) bytes={curBytes.Length}");
    }
}

static void WriteIco(string path, byte[] rgba, int w, int h, IcoImageEncoding encoding)
{
    var frame = new IcoFrame(rgba, w, h, PixelFormat.Rgba32, encoding);
    byte[] ico = IcoEncoder.Encode([frame]);
    File.WriteAllBytes(path, ico);
    Console.WriteLine($"# gen {Path.GetFileName(path)} {w}x{h} enc={encoding} bytes={ico.Length}");
}

/// 确定性打孔 ~1/7 像素 alpha=0（其余 255），保证 alpha ∈ {0,255}。
static byte[] MakeTransparent(byte[] rgba, int w, int h)
{
    var trp = (byte[])rgba.Clone();
    for (int i = 0; i < w * h; i++)
    {
        if (((i * 7919 + (i / w) * 104729) & 0x7) == 0)
            trp[i * 4 + 3] = 0;
    }
    return trp;
}

/// 合成 screen：1920×1080 低色数 UI 风（色块/分级渐变/条纹/字符，恒 ≤255 不透明色）。
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
    Fill(0, 0, w, 120, 0x3a, 0x42, 0x50);
    Fill(0, 120, 320, h, 0x2f, 0x35, 0x42);
    Fill(320, 120, w, 1080, 0x27, 0x2b, 0x33);
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
    // 顶栏字符 "ICO"（5×7 点阵，放大 8 倍）。
    DrawText(px, w, 60, 32, 8, FontGlyph('I'));
    DrawText(px, w, 60 + 8 * 5 + 24, 32, 8, FontGlyph('C'));
    DrawText(px, w, 60 + 2 * (8 * 5 + 24), 32, 8, FontGlyph('O'));
    return px;
}

// 5×7 点阵（0=空 1=前景 0xf1 0xc4 0x0f）。
static string[] FontGlyph(char c) => c switch
{
    'I' => new[] { "11111", "00100", "00100", "00100", "00100", "00100", "11111" },
    'C' => new[] { "01110", "10001", "10000", "10000", "10000", "10001", "01110" },
    'O' => new[] { "01110", "10001", "10001", "10001", "10001", "10001", "01110" },
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
    Console.Error.WriteLine("用法: IcoRef <ref|gen> ...");
    return 2;
}

static void Usage()
{
    Console.Error.WriteLine("用法: IcoRef <ref|gen> ...");
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
