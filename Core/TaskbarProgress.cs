using System.ComponentModel;
using System.Runtime.InteropServices;

namespace CFUnpacker.Core;

internal sealed class TaskbarProgress : IDisposable
{
    private static readonly UIntPtr SubclassId = new(1);

    private readonly IntPtr _windowHandle;
    private readonly ITaskbarList3 _taskbar;
    private readonly uint _taskbarButtonCreatedMessage;
    private readonly SubclassProcedure _subclassProcedure;
    private bool _taskbarButtonReady;
    private bool _showProgress;
    private ulong _completed;
    private bool _disposed;

    private TaskbarProgress(IntPtr windowHandle)
    {
        _windowHandle = windowHandle;
        _taskbar = (ITaskbarList3)new CTaskbarList();
        Marshal.ThrowExceptionForHR(_taskbar.HrInit());

        _taskbarButtonCreatedMessage = RegisterWindowMessage("TaskbarButtonCreated");
        if (_taskbarButtonCreatedMessage == 0)
        {
            throw new Win32Exception(Marshal.GetLastWin32Error());
        }

        _subclassProcedure = WindowProcedure;
        if (!SetWindowSubclass(_windowHandle, _subclassProcedure, SubclassId, IntPtr.Zero))
        {
            throw new Win32Exception(Marshal.GetLastWin32Error());
        }
    }

    public static TaskbarProgress? TryCreate(IntPtr windowHandle)
    {
        if (windowHandle == IntPtr.Zero)
        {
            return null;
        }

        try
        {
            return new TaskbarProgress(windowHandle);
        }
        catch (COMException)
        {
            return null;
        }
        catch (InvalidCastException)
        {
            return null;
        }
        catch (Win32Exception)
        {
            return null;
        }
    }

    public void Report(double percent)
    {
        if (_disposed)
        {
            return;
        }

        _completed = (ulong)Math.Round(Math.Clamp(percent, 0, 100));
        _showProgress = true;
        ApplyState();
    }

    public void Clear()
    {
        if (_disposed)
        {
            return;
        }

        _showProgress = false;
        ApplyState();
    }

    public void Dispose()
    {
        if (_disposed)
        {
            return;
        }

        Clear();
        RemoveWindowSubclass(_windowHandle, _subclassProcedure, SubclassId);
        Marshal.FinalReleaseComObject(_taskbar);
        _disposed = true;
    }

    private IntPtr WindowProcedure(
        IntPtr windowHandle,
        uint message,
        IntPtr wParam,
        IntPtr lParam,
        UIntPtr _,
        IntPtr __)
    {
        if (message == _taskbarButtonCreatedMessage)
        {
            _taskbarButtonReady = true;
            ApplyState();
        }

        return DefSubclassProc(windowHandle, message, wParam, lParam);
    }

    private void ApplyState()
    {
        if (_disposed || !_taskbarButtonReady)
        {
            return;
        }

        try
        {
            if (_showProgress)
            {
                Marshal.ThrowExceptionForHR(
                    _taskbar.SetProgressState(_windowHandle, TaskbarProgressState.Normal));
                Marshal.ThrowExceptionForHR(
                    _taskbar.SetProgressValue(_windowHandle, _completed, 100));
            }
            else
            {
                Marshal.ThrowExceptionForHR(
                    _taskbar.SetProgressState(_windowHandle, TaskbarProgressState.NoProgress));
            }
        }
        catch (COMException)
        {
            // Explorer can restart while an unpack operation is running.
        }
    }

    private enum TaskbarProgressState : uint
    {
        NoProgress = 0,
        Normal = 0x2,
    }

    [UnmanagedFunctionPointer(CallingConvention.Winapi)]
    private delegate IntPtr SubclassProcedure(
        IntPtr windowHandle,
        uint message,
        IntPtr wParam,
        IntPtr lParam,
        UIntPtr subclassId,
        IntPtr referenceData);

    [DllImport("user32.dll", CharSet = CharSet.Unicode, SetLastError = true)]
    private static extern uint RegisterWindowMessage(string message);

    [DllImport("comctl32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetWindowSubclass(
        IntPtr windowHandle,
        SubclassProcedure procedure,
        UIntPtr subclassId,
        IntPtr referenceData);

    [DllImport("comctl32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool RemoveWindowSubclass(
        IntPtr windowHandle,
        SubclassProcedure procedure,
        UIntPtr subclassId);

    [DllImport("comctl32.dll")]
    private static extern IntPtr DefSubclassProc(
        IntPtr windowHandle,
        uint message,
        IntPtr wParam,
        IntPtr lParam);

    [ComImport]
    [Guid("56FDF344-FD6D-11D0-958A-006097C9A090")]
    private class CTaskbarList
    {
    }

    [ComImport]
    [Guid("EA1AFB91-9E28-4B86-90E9-9E9F8A5EEA84")]
    [InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    private interface ITaskbarList3
    {
        [PreserveSig]
        int HrInit();

        [PreserveSig]
        int AddTab(IntPtr windowHandle);

        [PreserveSig]
        int DeleteTab(IntPtr windowHandle);

        [PreserveSig]
        int ActivateTab(IntPtr windowHandle);

        [PreserveSig]
        int SetActiveAlt(IntPtr windowHandle);

        [PreserveSig]
        int MarkFullscreenWindow(IntPtr windowHandle, [MarshalAs(UnmanagedType.Bool)] bool isFullscreen);

        [PreserveSig]
        int SetProgressValue(IntPtr windowHandle, ulong completed, ulong total);

        [PreserveSig]
        int SetProgressState(IntPtr windowHandle, TaskbarProgressState state);
    }
}
