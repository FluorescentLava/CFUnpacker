using System.Diagnostics;
using CFUnpacker.Core;
using CFUnpacker.Models;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.Windows.Storage.Pickers;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage;

namespace CFUnpacker;

public sealed partial class MainPage : Page
{
    private static readonly IReadOnlyList<GameChoice> GameChoices =
    [
        new(null, "自动识别"),
        .. GameProfile.All.Select(profile => new GameChoice(profile, profile.DisplayName)),
    ];

    private readonly ApkUnpacker _unpacker = new();
    private CancellationTokenSource? _cancellation;
    private string? _apkPath;
    private string? _lastOutputPath;
    private string? _lastLogMessage;
    private bool _isProgressDialogOpen;
    private bool _isStarting;

    public MainPage()
    {
        InitializeComponent();
        GameComboBox.ItemsSource = GameChoices;
        GameComboBox.SelectedIndex = 0;
    }

    private async void BrowseInputFolderButton_Click(object sender, RoutedEventArgs e)
    {
        PickFolderResult? result = await PickFolderAsync("选择输入文件夹", PickerLocationId.Downloads);
        if (result is not null)
        {
            SetInputFolder(result.Path);
        }
    }

    private async void BrowseOutputButton_Click(object sender, RoutedEventArgs e)
    {
        PickFolderResult? result = await PickFolderAsync("选择输出文件夹", PickerLocationId.Desktop);
        if (result is not null)
        {
            OutputPathTextBox.Text = result.Path;
        }
    }

    private async void ApkDropTarget_Click(object sender, RoutedEventArgs e)
    {
        var picker = new FileOpenPicker(App.MainWindow.AppWindow.Id)
        {
            CommitButtonText = "选择 APK",
            SuggestedStartLocation = PickerLocationId.Downloads,
        };
        picker.FileTypeFilter.Add(".apk");
        PickFileResult? result = await picker.PickSingleFileAsync();
        if (result is not null)
        {
            SetApkPath(result.Path);
        }
    }

    private async Task<PickFolderResult?> PickFolderAsync(string commitButtonText, PickerLocationId location)
    {
        var picker = new FolderPicker(App.MainWindow.AppWindow.Id)
        {
            CommitButtonText = commitButtonText,
            SuggestedStartLocation = location,
        };
        return await picker.PickSingleFolderAsync();
    }

    private void DropZone_DragOver(object sender, DragEventArgs e)
    {
        e.AcceptedOperation = DataPackageOperation.Copy;
        e.DragUIOverride.Caption = "使用此 APK";
        e.DragUIOverride.IsCaptionVisible = true;
        DropHintText.Text = "松开以选择 APK";
    }

    private void DropZone_DragLeave(object sender, DragEventArgs e) => ResetDropZone();

    private async void DropZone_Drop(object sender, DragEventArgs e)
    {
        ResetDropZone();
        if (!e.DataView.Contains(StandardDataFormats.StorageItems))
        {
            ShowInputError("拖入内容不是文件。");
            return;
        }

        IReadOnlyList<IStorageItem> items = await e.DataView.GetStorageItemsAsync();
        StorageFile? apk = items
            .OfType<StorageFile>()
            .FirstOrDefault(file =>
                string.Equals(Path.GetExtension(file.Path), ".apk", StringComparison.OrdinalIgnoreCase));
        if (apk is null)
        {
            ShowInputError("只能拖入一个 .apk 文件。");
            return;
        }

        SetApkPath(apk.Path);
    }

    private async void StartButton_Click(object sender, RoutedEventArgs e)
    {
        ResultInfoBar.IsOpen = false;
        OpenOutputButton.IsEnabled = false;
        _lastOutputPath = null;

        if (_isStarting ||
            !TryGetStartInfo(
                out GameChoice? choice,
                out string? apkPath,
                out string outputPath))
        {
            return;
        }

        _isStarting = true;
        GameComboBox.IsEnabled = false;
        StartButton.IsEnabled = false;
        ResolvedStart? resolved;
        try
        {
            resolved = await ResolveStartAsync(choice!, apkPath!, outputPath);
        }
        catch (Exception exception) when (
            exception is InvalidDataException or IOException or UnauthorizedAccessException)
        {
            ShowInputError($"APK 类型识别失败：{exception.Message}");
            return;
        }
        finally
        {
            _isStarting = false;
            GameComboBox.IsEnabled = true;
            StartButton.IsEnabled = true;
        }

        if (resolved is null)
        {
            return;
        }

        LogTextBox.Text = string.Empty;
        _lastLogMessage = null;
        AppendLog(resolved.DetectionLog);
        _cancellation = new CancellationTokenSource();
        SetBusy(true);
        var progress = new Progress<UnpackProgress>(UpdateProgress);

        try
        {
            UnpackResult result = await _unpacker.UnpackAsync(
                resolved.Request,
                progress,
                _cancellation.Token);
            _lastOutputPath = result.OutputPath;
            OpenOutputButton.IsEnabled = true;
            ResultInfoBar.Severity = InfoBarSeverity.Success;
            ResultInfoBar.Title = "解包完成";
            ResultInfoBar.Message =
                $"输出 {result.FramesWritten:N0} 张拆分 PNG，用时 {result.Elapsed:hh\\:mm\\:ss}。";
            ResultInfoBar.IsOpen = true;
            AppendLog(
                $"完成：提取 {result.AssetsExtracted:N0} 个资源，" +
                $"拆分 {result.AtlasesDecoded:N0} 个图集，跳过 {result.SkippedItems:N0} 项。");
        }
        catch (OperationCanceledException)
        {
            StatusTextBlock.Text = "已取消，暂存文件已清理";
            ResultInfoBar.Severity = InfoBarSeverity.Warning;
            ResultInfoBar.Title = "已取消";
            ResultInfoBar.Message = "未生成或替换最终输出目录。";
            ResultInfoBar.IsOpen = true;
            AppendLog("用户取消操作，暂存目录已清理。");
        }
        catch (Exception exception)
        {
            StatusTextBlock.Text = "解包失败";
            ResultInfoBar.Severity = InfoBarSeverity.Error;
            ResultInfoBar.Title = "解包失败";
            ResultInfoBar.Message = exception.Message;
            ResultInfoBar.IsOpen = true;
            AppendLog($"错误：{exception.Message}");
        }
        finally
        {
            _cancellation?.Dispose();
            _cancellation = null;
            SetBusy(false);
        }
    }

    private void ProgressDialog_PrimaryButtonClick(
        ContentDialog sender,
        ContentDialogButtonClickEventArgs args)
    {
        args.Cancel = true;
        CancelUnpack();
    }

    private async void ShowLogButton_Click(object sender, RoutedEventArgs e)
    {
        if (XamlRoot is null)
        {
            return;
        }

        LogDialog.XamlRoot = XamlRoot;
        await LogDialog.ShowAsync();
    }

    private void OpenOutputButton_Click(object sender, RoutedEventArgs e)
    {
        if (_lastOutputPath is null || !Directory.Exists(_lastOutputPath))
        {
            return;
        }

        Process.Start(new ProcessStartInfo("explorer.exe", $"\"{_lastOutputPath}\"")
        {
            UseShellExecute = true,
        });
    }

    private bool TryGetStartInfo(
        out GameChoice? choice,
        out string? apkPath,
        out string outputPath)
    {
        choice = GameComboBox.SelectedItem as GameChoice;
        apkPath = null;
        string inputFolder = InputFolderTextBox.Text.Trim().Trim('"');
        outputPath = OutputPathTextBox.Text.Trim().Trim('"');
        if (choice is null)
        {
            ShowInputError("请选择游戏版本。");
            return false;
        }

        if (!TryResolveApkPath(inputFolder, out apkPath, out string error))
        {
            ShowInputError(error);
            return false;
        }

        if (string.IsNullOrWhiteSpace(outputPath))
        {
            ShowInputError("请选择输出文件夹。");
            return false;
        }

        return true;
    }

    private async Task<ResolvedStart?> ResolveStartAsync(
        GameChoice choice,
        string apkPath,
        string outputPath)
    {
        ApkGameDetection detection = await ApkGameDetector.DetectAsync(apkPath);
        GameProfile? profile;
        bool forced = false;
        if (!detection.IsKnown)
        {
            if (choice.Profile is null)
            {
                ShowInputError(
                    $"无法自动识别 APK 类型。{detection.Evidence} " +
                    "请手动选择版本后再试。");
                return null;
            }

            ContentDialogResult result = await ShowUnknownProfileDialogAsync(
                choice.Profile,
                detection);
            if (result != ContentDialogResult.Primary)
            {
                return null;
            }

            profile = choice.Profile;
            forced = true;
        }
        else if (choice.Profile is null)
        {
            profile = detection.Profile;
        }
        else if (detection.IsCompatible(choice.Profile))
        {
            profile = choice.Profile;
        }
        else
        {
            ContentDialogResult result = await ShowProfileMismatchDialogAsync(
                choice.Profile,
                detection);
            if (result == ContentDialogResult.Primary)
            {
                profile = detection.Profile;
                SelectProfileChoice(profile!);
            }
            else if (result == ContentDialogResult.Secondary)
            {
                profile = choice.Profile;
                forced = true;
            }
            else
            {
                return null;
            }
        }

        string detectionLog = forced
            ? $"APK 识别警告：{detection.Evidence} 已按“{profile!.DisplayName}”强制解包。"
            : $"APK 识别：{profile!.DisplayName}，用时 {detection.Elapsed.TotalMilliseconds:F0} ms。{detection.Evidence}";
        return new ResolvedStart(
            new UnpackRequest(profile, apkPath, outputPath, OverwriteExisting: false),
            detectionLog);
    }

    private async Task<ContentDialogResult> ShowProfileMismatchDialogAsync(
        GameProfile selected,
        ApkGameDetection detection)
    {
        if (XamlRoot is null)
        {
            return ContentDialogResult.None;
        }

        ProfileMismatchDialog.XamlRoot = XamlRoot;
        ProfileMismatchDialog.Title = "APK 类型不一致";
        ProfileMismatchDialog.PrimaryButtonText = "切换并解包";
        ProfileMismatchDialog.SecondaryButtonText = "强制执行";
        ProfileMismatchText.Text =
            $"当前选择为“{selected.DisplayName}”，但资源识别结果为“{detection.Profile!.DisplayName}”。" +
            $"{Environment.NewLine}{Environment.NewLine}{detection.Evidence}";
        return await ProfileMismatchDialog.ShowAsync();
    }

    private async Task<ContentDialogResult> ShowUnknownProfileDialogAsync(
        GameProfile selected,
        ApkGameDetection detection)
    {
        if (XamlRoot is null)
        {
            return ContentDialogResult.None;
        }

        ProfileMismatchDialog.XamlRoot = XamlRoot;
        ProfileMismatchDialog.Title = "无法确认 APK 类型";
        ProfileMismatchDialog.PrimaryButtonText = "强制解包";
        ProfileMismatchDialog.SecondaryButtonText = string.Empty;
        ProfileMismatchText.Text =
            $"未能验证该 APK 与“{selected.DisplayName}”匹配。" +
            $"{Environment.NewLine}{Environment.NewLine}{detection.Evidence}";
        return await ProfileMismatchDialog.ShowAsync();
    }

    private void SelectProfileChoice(GameProfile profile)
    {
        GameChoice? choice = GameChoices.FirstOrDefault(
            item => item.Profile?.Kind == profile.Kind);
        if (choice is not null)
        {
            GameComboBox.SelectedItem = choice;
        }
    }

    private bool TryResolveApkPath(string inputFolder, out string? apkPath, out string error)
    {
        apkPath = null;
        error = string.Empty;
        if (!Directory.Exists(inputFolder))
        {
            error = "请选择有效的输入文件夹。";
            return false;
        }

        if (File.Exists(_apkPath) &&
            string.Equals(
                Path.GetDirectoryName(_apkPath),
                inputFolder,
                StringComparison.OrdinalIgnoreCase))
        {
            apkPath = _apkPath;
            return true;
        }

        string[] candidates;
        try
        {
            candidates = Directory
                .EnumerateFiles(inputFolder, "*.apk", SearchOption.TopDirectoryOnly)
                .Take(2)
                .ToArray();
        }
        catch (Exception exception) when (exception is IOException or UnauthorizedAccessException)
        {
            error = "无法读取输入文件夹。";
            return false;
        }

        if (candidates.Length == 1)
        {
            SetApkPath(candidates[0]);
            apkPath = candidates[0];
            return true;
        }

        error = candidates.Length == 0
            ? "输入文件夹中没有 .apk 文件。"
            : "输入文件夹中有多个 APK，请拖入要解包的文件。";
        return false;
    }

    private void SetInputFolder(string folder)
    {
        InputFolderTextBox.Text = folder;
        if (string.IsNullOrWhiteSpace(OutputPathTextBox.Text))
        {
            OutputPathTextBox.Text = folder;
        }

        try
        {
            string[] candidates = Directory
                .EnumerateFiles(folder, "*.apk", SearchOption.TopDirectoryOnly)
                .Take(2)
                .ToArray();
            if (candidates.Length == 1)
            {
                SetApkPath(candidates[0]);
            }
            else
            {
                _apkPath = null;
                DropHintText.Text = candidates.Length == 0
                    ? "该文件夹中没有 APK"
                    : "该文件夹中有多个 APK，请拖入文件";
            }
        }
        catch (Exception exception) when (exception is IOException or UnauthorizedAccessException)
        {
            _apkPath = null;
            DropHintText.Text = "无法读取该文件夹";
        }
    }

    private void SetApkPath(string path)
    {
        _apkPath = path;
        DropHintText.Text = $"已选择：{Path.GetFileName(path)}";

        string inputFolder = Path.GetDirectoryName(path) ?? string.Empty;
        InputFolderTextBox.Text = inputFolder;
        if (string.IsNullOrWhiteSpace(OutputPathTextBox.Text))
        {
            OutputPathTextBox.Text = inputFolder;
        }
    }

    private void SetBusy(bool busy)
    {
        GameComboBox.IsEnabled = !busy;
        InputFolderTextBox.IsEnabled = !busy;
        OutputPathTextBox.IsEnabled = !busy;
        BrowseInputFolderButton.IsEnabled = !busy;
        BrowseOutputButton.IsEnabled = !busy;
        ApkDropTarget.IsEnabled = !busy;
        ApkDropTarget.AllowDrop = !busy;
        ShowLogButton.IsEnabled = !busy;
        StartButton.IsEnabled = !busy;

        if (busy)
        {
            UnpackProgressBar.Value = 0;
            ProgressTextBlock.Text = "0%";
            StatusTextBlock.Text = "正在准备…";
            ProgressDialog.IsPrimaryButtonEnabled = true;
            ShowProgressDialog();
            App.MainWindow.TaskbarProgressReporter?.Report(0);
        }
        else
        {
            if (_isProgressDialogOpen)
            {
                ProgressDialog.Hide();
            }

            App.MainWindow.TaskbarProgressReporter?.Clear();
        }
    }

    private void ShowProgressDialog()
    {
        if (XamlRoot is null || _isProgressDialogOpen)
        {
            return;
        }

        ProgressDialog.XamlRoot = XamlRoot;
        _isProgressDialogOpen = true;
        _ = ShowProgressDialogAsync();
    }

    private async Task ShowProgressDialogAsync()
    {
        try
        {
            await ProgressDialog.ShowAsync();
        }
        finally
        {
            _isProgressDialogOpen = false;
        }
    }

    private void UpdateProgress(UnpackProgress value)
    {
        double percent = Math.Clamp(value.Percent, 0, 100);
        UnpackProgressBar.Value = percent;
        ProgressTextBlock.Text = $"{percent:0}%";
        StatusTextBlock.Text = value.Message;
        App.MainWindow.TaskbarProgressReporter?.Report(percent);
        if (!string.Equals(_lastLogMessage, value.Message, StringComparison.Ordinal))
        {
            _lastLogMessage = value.Message;
            AppendLog(value.Message);
        }
    }

    private void ResetDropZone()
    {
        DropHintText.Text = _apkPath is null
            ? "将 .apk 文件拖到此处"
            : $"已选择：{Path.GetFileName(_apkPath)}";
    }

    private void ShowInputError(string message)
    {
        ResultInfoBar.Severity = InfoBarSeverity.Error;
        ResultInfoBar.Title = "无法开始";
        ResultInfoBar.Message = message;
        ResultInfoBar.IsOpen = true;
    }

    private void AppendLog(string message)
    {
        string line = $"[{DateTime.Now:HH:mm:ss}] {message}";
        LogTextBox.Text = string.IsNullOrEmpty(LogTextBox.Text)
            ? line
            : $"{LogTextBox.Text}{Environment.NewLine}{line}";
    }

    private void CancelUnpack()
    {
        ProgressDialog.IsPrimaryButtonEnabled = false;
        StatusTextBlock.Text = "正在取消并清理…";
        _cancellation?.Cancel();
    }

    private sealed record GameChoice(GameProfile? Profile, string DisplayName)
    {
        public override string ToString() => DisplayName;
    }

    private sealed record ResolvedStart(UnpackRequest Request, string DetectionLog);
}
