// TIFF C# runner（单子命令）。
//
// `ref <corpusRoot> <refOutDir>` —— 单核参照测量：对语料目录（<corpusRoot>/TIFF 下的每个 *.tif）
// 用 Sdcb.SimdCodecs.Tiff.TiffImageDecoder 计时（全集暖机 1 遍 + 每文件 warmup2 + N 轮中位，
// TIFF_REF_ROUNDS env 控制），并把该文件的 RGBA8（紧密 4B/px、w*h*4）解码字节 dump 到
// <refOut>/<stem>.<w>x<h>.rgba（tiff_csharp_parity 的字节恒等参照）。
// 表行 `| `name` | ms |` 供 compare_refs.py 消费。
//
// 语料由 Rust 侧生成器（Benchmarks/tiff/gen，tiff crate 编码器）产生 —— 这里**不做 gen**：
// C# Sdcb TiffWriter 压缩只支持 Uncompressed/Deflate，产不出 LZW/PackBits 矩阵。
// 说明见 Benchmarks/tiff/README.md。

using System.Diagnostics;
using System.Globalization;
using Sdcb.SimdCodecs.Abstractions;
using Sdcb.SimdCodecs.Tiff;

if (args.Length < 1)
{
    Usage();
    return 2;
}

return args[0] switch
{
    "ref" => RunRef(args),
    _ => UsageReturn(),
};

static int RunRef(string[] args)
{
    if (args.Length < 3)
    {
        Console.Error.WriteLine("用法: TiffRef ref <corpusRoot> <refOutDir>");
        return 2;
    }
    string corpusRoot = args[1];
    string refOutDir = args[2];
    string tiffDir = Path.Combine(corpusRoot, "TIFF");
    if (!Directory.Exists(tiffDir))
    {
        Console.Error.WriteLine($"# ERR corpus TIFF 不存在: {tiffDir}");
        return 2;
    }
    Directory.CreateDirectory(refOutDir);

    int rounds = 9;
    if (Environment.GetEnvironmentVariable("TIFF_REF_ROUNDS") is { Length: > 0 } rs
        && int.TryParse(rs, NumberStyles.Integer, CultureInfo.InvariantCulture, out int rn) && rn > 0)
    {
        rounds = rn;
    }

    var files = Directory.EnumerateFiles(tiffDir, "*.tif", SearchOption.TopDirectoryOnly)
        .Concat(Directory.EnumerateFiles(tiffDir, "*.tiff", SearchOption.TopDirectoryOnly))
        .OrderBy(f => f, StringComparer.Ordinal)
        .ToList();

    // 全集暖机 1 遍：JIT/Tiered 提升在计时前全部收敛（对齐 jpeg/png/bmp 2026-11-08 公平化姿势）。
    foreach (var path in files)
    {
        try
        {
            byte[] wdata = File.ReadAllBytes(path);
            if (TiffImageDecoder.TryReadInfo(wdata, out ImageInfo wi))
            {
                _ = TiffImageDecoder.TryDecodeIntoRgba8(wdata, new byte[checked(wi.Width * wi.Height * 4)]);
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

        if (!TiffImageDecoder.TryReadInfo(data, out ImageInfo info))
        {
            Console.WriteLine($"# ERR {name} TryReadInfo 失败");
            continue;
        }
        int w = info.Width, h = info.Height;
        int len = checked(w * h * 4);
        var dest = new byte[len];

        if (!TiffImageDecoder.TryDecodeIntoRgba8(data, dest))
        {
            Console.WriteLine($"# ERR {name} TryDecodeIntoRgba8 失败");
            continue;
        }

        double ms = MedianMs(() =>
        {
            if (!TiffImageDecoder.TryDecodeIntoRgba8(data, dest))
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

static int UsageReturn()
{
    Console.Error.WriteLine("用法: TiffRef <ref> <corpusRoot> <refOutDir>");
    return 2;
}

static void Usage()
{
    Console.Error.WriteLine("用法: TiffRef <ref> <corpusRoot> <refOutDir>");
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
