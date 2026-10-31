// LittleDB 安全启动器
// 双击运行：自动生成/读取本机令牌，带 --token 启动 littledb 服务，
// 然后打开带令牌的 Web 控制台。关闭本窗口即停止服务。
// 测试时设置环境变量 LDB_NO_OPEN=1 可跳过自动打开浏览器。
//
// 编译：C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe /nologo /optimize /out:LittleDB控制台.exe launcher.cs

using System;
using System.Diagnostics;
using System.IO;
using System.Net.Sockets;
using System.Security.Cryptography;
using System.Threading;

class Launcher
{
    const int HttpPort = 7802;

    static string GenToken()
    {
        using (var rng = new RNGCryptoServiceProvider())
        {
            var bytes = new byte[20];
            rng.GetBytes(bytes);
            var sb = new System.Text.StringBuilder(40);
            foreach (var b in bytes) sb.Append(b.ToString("x2"));
            return sb.ToString();
        }
    }

    static string LoadOrCreateToken()
    {
        var path = ".ldb-token";
        if (File.Exists(path))
        {
            var t = File.ReadAllText(path).Trim();
            if (t.Length > 0) return t;
        }
        var token = GenToken();
        try { File.WriteAllText(path, token); }
        catch (Exception e) { Console.WriteLine("警告：无法写入令牌文件 .ldb-token（" + e.Message + "），本次使用临时令牌"); }
        return token;
    }

    static bool WaitPortReady()
    {
        for (int i = 0; i < 100; i++)
        {
            try { new TcpClient("127.0.0.1", HttpPort).Close(); return true; }
            catch { Thread.Sleep(200); }
        }
        return false;
    }

    static void Main()
    {
        var token = LoadOrCreateToken();

        Console.WriteLine("LittleDB 正在启动（已启用访问令牌保护）...");
        Console.WriteLine("------------------------------------------------");
        Console.WriteLine("  协议端口 : 127.0.0.1:7801");
        Console.WriteLine("  控制台   : http://127.0.0.1:" + HttpPort);
        Console.WriteLine("  数据目录 : ./data");
        Console.WriteLine("------------------------------------------------");
        Console.WriteLine("关闭本窗口即可停止服务。");
        Console.WriteLine();

        Process child;
        try
        {
            var startInfo = new ProcessStartInfo
            {
                FileName = "./littledb.exe",
                Arguments = "serve ./data",
                UseShellExecute = false,
            };
            // 令牌走环境变量而不是命令行参数：
            // 命令行会被任务管理器、进程审计、PowerShell 历史记录看到
            startInfo.EnvironmentVariables["LDB_TOKEN"] = token;
            child = Process.Start(startInfo);
        }
        catch (Exception e)
        {
            Console.WriteLine("启动 littledb.exe 失败：" + e.Message);
            Console.WriteLine("请确认本启动器与 littledb.exe 在同一目录。");
            Console.ReadLine();
            return;
        }

        var url = "http://127.0.0.1:" + HttpPort + "/?token=" + token;
        new Thread(delegate ()
        {
            if (WaitPortReady() && Environment.GetEnvironmentVariable("LDB_NO_OPEN") == null)
            {
                try { Process.Start(new ProcessStartInfo(url) { UseShellExecute = true }); }
                catch { }
            }
        }).Start();

        child.WaitForExit();
        Console.WriteLine();
        Console.WriteLine("服务已停止。按回车关闭窗口。");
        Console.ReadLine();
    }
}
