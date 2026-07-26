#include <windows.h>
#include <wchar.h>

static const wchar_t* SkipExecutableName(const wchar_t* commandLine)
{
    if (*commandLine == L'"')
    {
        commandLine++;
        while (*commandLine != L'\0' && *commandLine != L'"')
        {
            commandLine++;
        }

        if (*commandLine == L'"')
        {
            commandLine++;
        }
    }
    else
    {
        while (*commandLine != L'\0' && *commandLine != L' ' && *commandLine != L'\t')
        {
            commandLine++;
        }
    }

    while (*commandLine == L' ' || *commandLine == L'\t')
    {
        commandLine++;
    }

    return commandLine;
}

int WINAPI wWinMain(HINSTANCE instance, HINSTANCE previous, PWSTR commandLine, int showCommand)
{
    wchar_t launcherPath[MAX_PATH];
    DWORD length = GetModuleFileNameW(NULL, launcherPath, ARRAYSIZE(launcherPath));
    if (length == 0 || length == ARRAYSIZE(launcherPath))
    {
        MessageBoxW(NULL, L"Unable to locate the launcher.", L"CFUnpacker", MB_ICONERROR);
        return 1;
    }

    wchar_t* lastSeparator = wcsrchr(launcherPath, L'\\');
    if (lastSeparator == NULL)
    {
        MessageBoxW(NULL, L"Invalid launcher location.", L"CFUnpacker", MB_ICONERROR);
        return 1;
    }

    *lastSeparator = L'\0';
    wchar_t runtimeDirectory[MAX_PATH];
    if (swprintf_s(runtimeDirectory, ARRAYSIZE(runtimeDirectory), L"%s\\runtime", launcherPath) < 0)
    {
        return 1;
    }

    wchar_t childCommandLine[32768];
    const wchar_t* arguments = SkipExecutableName(GetCommandLineW());
    if (swprintf_s(
            childCommandLine,
            ARRAYSIZE(childCommandLine),
            L"\"%s\\CFUnpacker.exe\" %s",
            runtimeDirectory,
            arguments) < 0)
    {
        MessageBoxW(NULL, L"The command line is too long.", L"CFUnpacker", MB_ICONERROR);
        return 1;
    }

    STARTUPINFOW startupInfo = { 0 };
    startupInfo.cb = sizeof(startupInfo);
    PROCESS_INFORMATION processInfo = { 0 };
    if (!CreateProcessW(
            NULL,
            childCommandLine,
            NULL,
            NULL,
            FALSE,
            0,
            NULL,
            runtimeDirectory,
            &startupInfo,
            &processInfo))
    {
        MessageBoxW(
            NULL,
            L"The runtime folder is missing or cannot be started.",
            L"CFUnpacker",
            MB_ICONERROR);
        return 1;
    }

    CloseHandle(processInfo.hThread);
    WaitForSingleObject(processInfo.hProcess, INFINITE);
    DWORD exitCode = 1;
    GetExitCodeProcess(processInfo.hProcess, &exitCode);
    CloseHandle(processInfo.hProcess);
    return (int)exitCode;
}
