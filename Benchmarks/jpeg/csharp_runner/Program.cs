// JPEG C# runner：两个子命令。
//
// `ref <corpusDir> <refOutDir>` —— 单核参照生成器：对语料每张 jpg 用
// Sdcb.SimdCodecs.Jpeg.DecodeTo 计时（暖机 1 + N 轮中位），并 dump 一次解码字节到
// <refOut>/<stem>.<w>x<h>.{gray|rgb}（jpeg_csharp_parity 的字节恒等参照）。
//
// `encode <srcDir> <outDir>` —— 广谱语料生成器（B2 翻正前置验证）：对 srcDir 每个
// .jpg 用 JpegDecoder.Decode 解成 RGBA 栅格，再经 JpegEncoder.Encode 产出 6 个变体：
//   baseline-420-q90 / baseline-444-q90 / baseline-gray-q85 /
//   progressive-420-q80 / progressive-444-q80 / baseline-420-q80-rst16（RST 覆盖）
// 命名 <stem>__<variant>.jpg。质量/subsampling/progressive/RestartInterval 均走公开 API
// （JpegEncodeOptions，见 E:\Rust\SimdCodecs\src\Sdcb.SimdCodecs.Jpeg\JpegEncodeOptions.cs）。

using System.Diagnostics;
using System.Globalization;
using Sdcb.SimdCodecs.Jpeg;

if (args.Length < 1)
{
    Usage();
    return 2;
}

return args[0] switch
{
    "ref" => RunRef(args),
    "encode" => RunEncode(args),
    _ => UsageReturn(),
};

static int RunRef(string[] args)
{
    if (args.Length < 3)
    {
        Console.Error.WriteLine("用法: JpegRef ref <corpusDir> <refOutDir>");
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
    if (Environment.GetEnvironmentVariable("JPEG_REF_ROUNDS") is { Length: > 0 } rs
        && int.TryParse(rs, NumberStyles.Integer, CultureInfo.InvariantCulture, out int rn) && rn > 0)
    {
        rounds = rn;
    }

    var files = CollectFiles(corpusDir);

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

        Int32 w, h; bool gray; int bpp;
        try
        {
            JpegInfo info = JpegDecoder.ReadInfo(data, null);
            w = info.Width;
            h = info.Height;
            gray = info.Components == 1;
            bpp = gray ? 1 : 3;
        }
        catch (Exception ex)
        {
            Console.WriteLine($"# ERR {name} ReadInfo 失败: {ex.Message}");
            continue;
        }

        int stride = checked(w * bpp);
        int len = checked(stride * h);
        var dest = new byte[len];
        var format = gray ? JpegPixelFormat.Gray8 : JpegPixelFormat.Rgb24;

        bool decoded = false;
        try
        {
            JpegDecoder.DecodeTo(data, dest, stride, format, JpegScale.Full);
            decoded = true;
        }
        catch (Exception ex)
        {
            Console.WriteLine($"# ERR {name} DecodeTo 失败: {ex.Message}");
            continue;
        }

        double ms = MedianMs(() =>
        {
            _ = JpegDecoder.DecodeTo(data, dest, stride, format, JpegScale.Full);
        }, rounds);

        try
        {
            string ext = gray ? "gray" : "rgb";
            string refFile = Path.Combine(refOutDir, $"{stem}.{w}x{h}.{ext}");
            if (!decoded) { throw new InvalidOperationException("未解码成功"); }
            File.WriteAllBytes(refFile, dest);
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

static int RunEncode(string[] args)
{
    if (args.Length < 3)
    {
        Console.Error.WriteLine("用法: JpegRef encode <srcDir> <outDir>");
        return 2;
    }

    string srcDir = args[1];
    string outDir = args[2];
    if (!Directory.Exists(srcDir))
    {
        Console.Error.WriteLine($"# ERR src 不存在: {srcDir}");
        return 2;
    }
    Directory.CreateDirectory(outDir);

    // (名, 质量, subsampling, progressive, gray, restartInterval)
    var variants = new (string Name, int Quality, JpegSubsampling Sub, bool Progressive, bool Gray, int Restart)[]
    {
        ("baseline-420-q90", 90, JpegSubsampling.Chroma420, false, false, 0),
        ("baseline-444-q90", 90, JpegSubsampling.Chroma444, false, false, 0),
        ("baseline-gray-q85", 85, JpegSubsampling.Auto, false, true, 0),
        ("progressive-420-q80", 80, JpegSubsampling.Chroma420, true, false, 0),
        ("progressive-444-q80", 80, JpegSubsampling.Chroma444, true, false, 0),
        ("baseline-420-q80-rst16", 80, JpegSubsampling.Chroma420, false, false, 16),
    };

    var files = CollectFiles(srcDir);

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

        JpegImage img;
        try
        {
            img = JpegDecoder.Decode(data);
        }
        catch (Exception ex)
        {
            Console.WriteLine($"# ERR {name} Decode 失败: {ex.Message}");
            continue;
        }

        int w = img.Width, h = img.Height;
        byte[] rgba = img.Pixels;

        foreach (var v in variants)
        {
            byte[] pixels;
            int channels;
            if (v.Gray)
            {
                channels = 1;
                pixels = new byte[w * h];
                for (int i = 0; i < w * h; i++)
                {
                    int r = rgba[i * 4], g = rgba[i * 4 + 1], b = rgba[i * 4 + 2];
                    pixels[i] = (byte)((r * 299 + g * 587 + b * 114 + 500) / 1000);
                }
            }
            else
            {
                channels = 3;
                pixels = new byte[w * h * 3];
                for (int i = 0; i < w * h; i++)
                {
                    pixels[i * 3] = rgba[i * 4];
                    pixels[i * 3 + 1] = rgba[i * 4 + 1];
                    pixels[i * 3 + 2] = rgba[i * 4 + 2];
                }
            }

            var opts = new JpegEncodeOptions
            {
                Quality = v.Quality,
                Subsampling = v.Sub,
                Progressive = v.Progressive,
                RestartInterval = v.Restart,
            };

            try
            {
                byte[] jpg = JpegEncoder.Encode(pixels, w, h, channels, opts);
                string outPath = Path.Combine(outDir, $"{stem}__{v.Name}.jpg");
                File.WriteAllBytes(outPath, jpg);
            }
            catch (Exception ex)
            {
                Console.WriteLine($"# ERR {name} 变体 {v.Name} 编码失败: {ex.Message}");
            }
        }
        done++;
    }

    Console.WriteLine($"# DONE encode files={done} variants={variants.Length} src={srcDir} out={outDir}");
    return 0;
}

static List<string> CollectFiles(string dir) =>
    Directory.EnumerateFiles(dir, "*.jpg", SearchOption.TopDirectoryOnly)
        .Concat(Directory.EnumerateFiles(dir, "*.jpeg", SearchOption.TopDirectoryOnly))
        .OrderBy(f => f, StringComparer.Ordinal)
        .ToList();

static int UsageReturn()
{
    Console.Error.WriteLine("用法: JpegRef <ref|encode> <srcDir> <outDir>");
    return 2;
}

static void Usage()
{
    Console.Error.WriteLine("用法: JpegRef <ref|encode> <srcDir> <outDir>");
}

static double MedianMs(Action action, int rounds)
{
    action(); // 暖机 1 轮
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
