// LittleDB 桌面版：主界面壳。
//
// 设计：窗口内直接加载服务端自带的 Web 控制台（与浏览器访问 7802 完全相同的前端），
// 因此桌面端与网页端天然同步——同一份前端代码、同一个服务进程、同一份数据，
// 浏览器里的操作桌面端立即可见，反之亦然；控制台前端的更新不需要重新打包桌面壳。
//
// 壳本身只做三件原生的事：
//   1. 服务生命周期：启动/停止/重启 littledb.exe（令牌走 LDB_TOKEN 环境变量，不进命令行）；
//   2. 嵌入 WebView2 显示控制台；
//   3. 托盘最小化、状态栏、打开数据目录。

using System.Diagnostics;
using System.Security.Cryptography;
using System.Text;
using Microsoft.Web.WebView2.Core;
using Microsoft.Web.WebView2.WinForms;

namespace LittleDB.Desktop;

internal static class Program
{
    [STAThread]
    private static void Main()
    {
        ApplicationConfiguration.Initialize();
        Application.Run(new MainForm());
    }
}

internal sealed class MainForm : Form
{
    private readonly string _baseDir = AppContext.BaseDirectory;
    private readonly string _dataDir;
    private readonly string _tokenPath;
    private readonly int _protoPort = 7801;
    private readonly int _httpPort = 7802;

    private Process _server;
    private string _token;
    private readonly WebView2 _web;
    private readonly StatusStrip _status;
    private readonly ToolStripStatusLabel _stateLabel;
    private readonly ToolStripStatusLabel _portLabel;
    private readonly System.Windows.Forms.Timer _poll;
    private readonly NotifyIcon _tray;
    private bool _busy;

    public MainForm()
    {
        Text = "LittleDB 桌面版";
        Width = 1180;
        Height = 800;
        StartPosition = FormStartPosition.CenterScreen;
        Font = new Font("Microsoft YaHei UI", 9f);
        Icon = Icon.ExtractAssociatedIcon(Application.ExecutablePath);

        _dataDir = Path.Combine(_baseDir, "data");
        _tokenPath = Path.Combine(_baseDir, ".ldb-token");
        _token = LoadOrCreateToken();

        // ---- 状态栏（顶部）：状态 + 端口 + 服务操作
        var strip = new StatusStrip { SizingGrip = false };
        _stateLabel = new ToolStripStatusLabel("服务启动中…") { Spring = true, TextAlign = ContentAlignment.MiddleLeft };
        _portLabel = new ToolStripStatusLabel("");
        var restartBtn = new ToolStripButton("重启服务") { DisplayStyle = ToolStripItemDisplayStyle.Text };
        var dataBtn = new ToolStripButton("数据目录") { DisplayStyle = ToolStripItemDisplayStyle.Text };
        var browserBtn = new ToolStripButton("在浏览器打开") { DisplayStyle = ToolStripItemDisplayStyle.Text };
        restartBtn.Click += async (_, _) => { await RestartServerAsync(); };
        dataBtn.Click += (_, _) => { try { Process.Start("explorer.exe", _dataDir); } catch { /* 忽略 */ } };
        browserBtn.Click += (_, _) => OpenInBrowser();
        strip.Items.AddRange(new ToolStripItem[] { _stateLabel, _portLabel, restartBtn, dataBtn, browserBtn });
        _status = strip;

        // ---- WebView2：控制台本体
        _web = new WebView2 { Dock = DockStyle.Fill };
        Controls.Add(_web);
        Controls.Add(strip);

        // ---- 托盘
        _tray = new NotifyIcon
        {
            Icon = Icon,
            Text = "LittleDB",
            Visible = true,
        };
        _tray.DoubleClick += (_, _) => { Show(); WindowState = FormWindowState.Normal; Activate(); };
        var trayMenu = new ContextMenuStrip();
        trayMenu.Items.Add("显示主界面", null, (_, _) => { Show(); WindowState = FormWindowState.Normal; Activate(); });
        trayMenu.Items.Add("退出（停止服务）", null, (_, _) => Close());
        _tray.ContextMenuStrip = trayMenu;

        // 最小化到托盘
        Resize += (_, _) =>
        {
            if (WindowState == FormWindowState.Minimized)
            {
                Hide();
            }
        };
        FormClosing += async (_, e) =>
        {
            if (_busy) { e.Cancel = true; return; }
            _tray.Visible = false;
            StopServer();
        };

        _poll = new System.Windows.Forms.Timer { Interval = 2000 };
        _poll.Tick += async (_, _) => await RefreshStatusAsync();
        Load += async (_, _) => await BootAsync();
        _poll.Start();
    }

    // ------------------------------------------------------------ 启动/停止服务

    private string LoadOrCreateToken()
    {
        try
        {
            var existing = File.ReadAllText(_tokenPath).Trim();
            if (existing.Length >= 16) return existing;
        }
        catch { /* 不存在就生成 */ }

        var bytes = RandomNumberGenerator.GetBytes(24);
        var token = Convert.ToHexString(bytes).ToLowerInvariant();
        File.WriteAllText(_tokenPath, token, new UTF8Encoding(false));
        return token;
    }

    private static bool ProcessAlive(int pid)
    {
        try { using var p = Process.GetProcessById(pid); return !p.HasExited; }
        catch { return false; }
    }

    private int? RunningPid()
    {
        // 自己拉起的子进程优先（不依赖 pid 文件是否还在）
        try { if (_server != null && !_server.HasExited) return _server.Id; }
        catch { /* 进程已退出 */ }
        var path = Path.Combine(_dataDir, "littledb.pid");
        try
        {
            var pid = int.Parse(File.ReadAllText(path).Trim());
            return ProcessAlive(pid) ? pid : null;
        }
        catch { return null; }
    }

    private async Task BootAsync()
    {
        Directory.CreateDirectory(_dataDir);
        await InitWebAsync();
        await StartServerIfNeededAsync();
        await RefreshStatusAsync();
    }

    private async Task InitWebAsync()
    {
        var env = await CoreWebView2Environment.CreateAsync(
            userDataFolder: Path.Combine(_baseDir, "webview-data"));
        await _web.EnsureCoreWebView2Async(env);
        // 控制台自己的请求都在本源内；禁用外部导航弹窗行为保持简单
        _web.CoreWebView2.NewWindowRequested += (_, e) =>
        {
            // target=_blank 一律在主窗口打开（控制台目前没有外链，稳妥起见）
            e.Handled = true;
            _web.CoreWebView2.Navigate(e.Uri);
        };
    }

    private void NavigateToConsole()
    {
        // 令牌经 URL 交给控制台：它会存入 sessionStorage 并立刻清理地址栏（前端既有行为）
        _web.CoreWebView2.Navigate($"http://127.0.0.1:{_httpPort}/?token={_token}");
    }

    private async Task StartServerIfNeededAsync()
    {
        if (RunningPid() != null)
        {
            return; // 已有实例（比如之前 start 过）直接复用
        }
        await StartServerAsync();
    }

    private async Task StartServerAsync()
    {
        var exe = Path.Combine(_baseDir, "littledb.exe");
        if (!File.Exists(exe))
        {
            MessageBox.Show($"找不到 {exe}\n请确认 littledb.exe 与本程序在同一目录。", "LittleDB",
                MessageBoxButtons.OK, MessageBoxIcon.Warning);
            return;
        }

        var psi = new ProcessStartInfo
        {
            FileName = exe,
            Arguments = $"serve \"{_dataDir}\" --port {_protoPort} --http-port {_httpPort} --no-open",
            UseShellExecute = false,
            CreateNoWindow = true,
            WorkingDirectory = _baseDir,
        };
        // 令牌走环境变量（命令行对其它进程可见）
        psi.EnvironmentVariables["LDB_TOKEN"] = _token;

        _server = Process.Start(psi);
        // 写 pid 文件：与 `littledb start` 的约定一致，
        // 这样 CLI 的 stop/status 也能管到桌面壳拉起的服务
        try { File.WriteAllText(Path.Combine(_dataDir, "littledb.pid"), _server.Id.ToString()); }
        catch { }
        // 等服务就绪（最多 15 秒）
        using var http = new HttpClient();
        for (var i = 0; i < 75; i++)
        {
            try
            {
                using var request = new HttpRequestMessage(HttpMethod.Get,
                    $"http://127.0.0.1:{_httpPort}/api/health");
                request.Headers.Add("Authorization", "Bearer " + _token);
                using var response = await http.SendAsync(request);
                if (response.IsSuccessStatusCode) break;
            }
            catch { /* 未就绪，继续等 */ }
            await Task.Delay(200);
        }
    }

    private void StopServer()
    {
        var pid = RunningPid();
        if (pid == null) return;
        // 先温和关闭自己的子进程（让它走正常退出流程），失败再强杀
        if (_server != null && !_server.HasExited && _server.Id == pid)
        {
            try { _server.CloseMainWindow(); } catch { }
            _server.WaitForExit(3000);
            if (!_server.HasExited) { try { _server.Kill(true); } catch { } }
        }
        if (ProcessAlive(pid.Value))
        {
            try
            {
                using var killer = Process.Start(new ProcessStartInfo
                {
                    FileName = "taskkill",
                    Arguments = $"/PID {pid} /T /F",
                    UseShellExecute = false,
                    CreateNoWindow = true,
                });
                killer?.WaitForExit(5000);
            }
            catch { /* 尽力而为 */ }
        }
        try { File.Delete(Path.Combine(_dataDir, "littledb.pid")); } catch { }
    }

    private async Task RestartServerAsync()
    {
        _busy = true;
        try
        {
            StopServer();
            await Task.Delay(500);
            await StartServerAsync();
            NavigateToConsole();
        }
        finally { _busy = false; }
    }

    // ------------------------------------------------------------ 状态刷新

    private async Task RefreshStatusAsync()
    {
        var pid = RunningPid();
        bool healthy;
        try
        {
            using var http = new HttpClient { Timeout = TimeSpan.FromSeconds(2) };
            using var request = new HttpRequestMessage(HttpMethod.Get,
                $"http://127.0.0.1:{_httpPort}/api/health");
            request.Headers.Add("Authorization", "Bearer " + _token);
            using var response = await http.SendAsync(request);
            healthy = response.IsSuccessStatusCode;
        }
        catch { healthy = false; }

        _stateLabel.Text = healthy
            ? $"服务运行中（PID {pid}）"
            : (pid == null ? "服务未运行" : $"服务异常（PID {pid} 无响应）");
        _portLabel.Text = $"HTTP {_httpPort} · LDBP {_protoPort}";

        // 首页还没加载成功且服务已健康 → 加载控制台（或重启后重新加载）
        if (healthy && _web.CoreWebView2 != null &&
            (_web.Source == null || string.IsNullOrEmpty(_web.Source.ToString()) ||
             _web.Source.ToString() == "about:blank"))
        {
            NavigateToConsole();
        }
    }

    private void OpenInBrowser()
    {
        try
        {
            // 用同一令牌在浏览器里打开同一控制台（与桌面端同步查看）
            Process.Start(new ProcessStartInfo
            {
                FileName = $"http://127.0.0.1:{_httpPort}/?token={_token}",
                UseShellExecute = true,
            });
        }
        catch { }
    }
}
