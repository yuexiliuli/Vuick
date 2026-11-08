// PNG C# runner：两个子命令。
//
// `ref <corpusDir> <refOutDir>` —— 单核参照测量 / 生成器：对语料每张 .png 用
// Sdcb.SimdCodecs.Png.PngImageDecoder.TryDecodeIntoRgba8 计时（全集暖机 1 遍 + 每文件
// warmup2 + N 轮中位，PNG_REF_ROUNDS env 控制），并把该文件的 RGBA8（紧密 4B/px、w*h*4）
// 解码字节 dump 到 <refOut>/<stem>.<w>x<h>.rgba（png_csharp_parity 的字节恒等参照）。
// 表行 `| `name` | ms |` 供 compare_refs.py 消费。
//
// `gen <srcPng> <outDir> <tag>` —— 比较集扩充生成器：把 srcPng 用 TryDecodeRgba8 解成
// RGBA8，再经 PngWriter 产出两个保真变体（RGBA8 布局，解码回读与源逐字节一致）：
//   <stem>__<tag>_rgba8.png  —— PngWriter.Encode（非隔行 RGBA8，Sdcb 自家压缩流）
//   <stem>__<tag>_p8a.png    —— PngWriter.Encode + LosslessReduction（≤256 色走 PLTE+tRNS，
//                                否则保持真彩；解码 RGBA8 不变）
// 变体放进语料目录后即被 ref（计时表）+ probe（ours）+ parity（字节对照）统一收进比较集。

using System.Diagnostics;
using System.Globalization;
using Sdcb.SimdCodecs.Abstractions;
using Sdcb.SimdCodecs.Png;

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
        Console.Error.WriteLine("用法: PngRef ref <corpusDir> <refOutDir>");
        return 2;
    }
    string corpusDir = args[1];
    string refOutDir = args[2];
    if (!Directory.Exists(corpusDir))
    {
        Console.Error.WriteLine($"# ERR corpus 不存在: {corpusDir}");
        return 2;
    }
    Directory.CreateDirectory(refOutDir);

    int rounds = 9;
    if (Environment.GetEnvironmentVariable("PNG_REF_ROUNDS") is { Length: > 0 } rs
        && int.TryParse(rs, NumberStyles.Integer, CultureInfo.InvariantCulture, out int rn) && rn > 0)
    {
        rounds = rn;
    }

    var files = CollectFiles(corpusDir);

    // 全集暖机 1 遍：JIT/Tiered 提升在计时前全部收敛（对齐 jpeg 2026-11-08 公平化姿势）。
    foreach (var path in files)
    {
        try
        {
            byte[] wdata = File.ReadAllBytes(path);
            if (PngImageDecoder.TryReadInfo(wdata, out ImageInfo wi))
            {
                _ = PngImageDecoder.TryDecodeIntoRgba8(wdata, new byte[checked(wi.Width * wi.Height * 4)]);
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

        if (!PngImageDecoder.TryReadInfo(data, out ImageInfo info))
        {
            Console.WriteLine($"# ERR {name} TryReadInfo 失败");
            continue;
        }
        int w = info.Width, h = info.Height;
        int len = checked(w * h * 4);
        var dest = new byte[len];

        if (!PngImageDecoder.TryDecodeIntoRgba8(data, dest))
        {
            Console.WriteLine($"# ERR {name} TryDecodeIntoRgba8 失败");
            continue;
        }

        double ms = MedianMs(() =>
        {
            if (!PngImageDecoder.TryDecodeIntoRgba8(data, dest))
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
    if (args.Length < 4)
    {
        Console.Error.WriteLine("用法: PngRef gen <srcPng> <outDir> <tag>");
        return 2;
    }
    string srcPath = args[1];
    string outDir = args[2];
    string tag = args[3];
    if (!File.Exists(srcPath))
    {
        Console.Error.WriteLine($"# ERR src 不存在: {srcPath}");
        return 2;
    }
    Directory.CreateDirectory(outDir);
    string stem = Path.GetFileNameWithoutExtension(srcPath);

    byte[] data;
    try
    {
        data = File.ReadAllBytes(srcPath);
    }
    catch (Exception ex)
    {
        Console.WriteLine($"# ERR {stem} 读文件失败: {ex.Message}");
        return 1;
    }

    if (!PngImageDecoder.TryDecodeRgba8(data, out byte[] rgba, out int w, out int h))
    {
        Console.WriteLine($"# ERR {stem} TryDecodeRgba8 失败");
        return 1;
    }

    byte[] rgba8 = PngWriter.Encode(rgba, w, h);
    File.WriteAllBytes(Path.Combine(outDir, $"{stem}__{tag}_rgba8.png"), rgba8);
    byte[] p8a = PngWriter.Encode(rgba, w, h, new PngWriteOptions { LosslessReduction = true });
    File.WriteAllBytes(Path.Combine(outDir, $"{stem}__{tag}_p8a.png"), p8a);
    Console.WriteLine($"# GEN {stem} {w}x{h} rgba8={rgba8.Length}B p8a={p8a.Length}B");
    return 0;
}

static List<string> CollectFiles(string dir) =>
    Directory.EnumerateFiles(dir, "*.png", SearchOption.TopDirectoryOnly)
        .OrderBy(f => f, StringComparer.Ordinal)
        .ToList();

static int UsageReturn()
{
    Console.Error.WriteLine("用法: PngRef <ref|gen> ...");
    return 2;
}

static void Usage()
{
    Console.Error.WriteLine("用法: PngRef <ref|gen> ...");
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
