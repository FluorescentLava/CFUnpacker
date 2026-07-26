using Microsoft.UI.Xaml;
using Windows.Graphics;
using CarrotUnpacker.Core;

namespace CarrotUnpacker;

public sealed partial class MainWindow : Window
{
    internal TaskbarProgress? TaskbarProgressReporter { get; }

    public MainWindow()
    {
        InitializeComponent();
        TaskbarProgressReporter = TaskbarProgress.TryCreate(
            WinRT.Interop.WindowNative.GetWindowHandle(this));
        Closed += (_, _) => TaskbarProgressReporter?.Dispose();
        ExtendsContentIntoTitleBar = true;
        SetTitleBar(AppTitleBar);
        AppWindow.SetIcon("Assets/AppIcon.ico");
        AppWindow.Resize(new SizeInt32(900, 1024));
        RootFrame.Navigate(typeof(MainPage));
    }
}
