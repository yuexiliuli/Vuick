// JPEG C# 单核参照生成器：对语料每张 jpg 用 Sdcb.SimdCodecs.Jpeg.DecodeTo 计时（暖机 1 + N 轮中位），
// 并 dump 一次解码字节到 <refOut>/<stem>.<w>x<h>.{gray|rgb}（jpeg_csharp_parity 的字节恒等参照）。
//
// 用法：JpegRef ref <corpusDir> <refOutDir>
//   - 每文件：`JpegDecoder.ReadInfo(data, null)` 取 (Width, Height, Components, Progressive)
//   - 颜色：Components==1 → JpegPixelFormat.Gray8（1B/px）；否则 Rgb24（3B/px）；stride = w*bpp
//   - 计时：DecodeTo(data, dest, stride, format, JpegScale.Full)；env JPEG_REF_ROUNDS 覆盖轮数（缺省 9，另 +1 暖机）
//   - stdout：`#` 注释头 + 每文件一行 `` | `name` | <ms> | ``（InvariantCulture，3 位小数）
//   - 单文件失败打印 `# ERR <name> <reason>` 继续；正常结束返回 0
// 参照字节 dump：取最后一次 DecodeTo 的输出原样写盘（灰→.gray、彩→.rgb）。

using System.Diagnostics;
using System.Globalization;
using Sdcb.SimdCodecs.Jpeg;

if (args.Length < 3 || args[0] != "ref")
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

// 参照表头由 run_csharp_ref.ps1 写入（权威）；本程序只输出数据行与 #-注释（# ERR / # DONE）。

var files = Directory.EnumerateFiles(corpusDir, "*.jpg", SearchOption.TopDirectoryOnly)
    .Concat(Directory.EnumerateFiles(corpusDir, "*.jpeg", SearchOption.TopDirectoryOnly))
    .OrderBy(f => f, StringComparer.Ordinal)
    .ToArray();

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
        // 暖机 1 轮
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

    // 参照 dump：取一次解码输出原样写盘
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

Console.WriteLine($"# DONE files={done} total={files.Length}");
return 0;

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
