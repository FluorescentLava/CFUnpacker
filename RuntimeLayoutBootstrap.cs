using System.Reflection;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Runtime.Loader;

namespace CFUnpacker;

internal static class RuntimeLayoutBootstrap
{
    [ModuleInitializer]
    internal static void Initialize()
    {
        var runtimeDirectory = Path.Combine(AppContext.BaseDirectory, "runtime");
        if (!Directory.Exists(runtimeDirectory))
        {
            return;
        }

        SetDllDirectory(runtimeDirectory);
        AssemblyLoadContext.Default.Resolving += (_, assemblyName) =>
        {
            var candidate = Path.Combine(runtimeDirectory, $"{assemblyName.Name}.dll");
            return File.Exists(candidate)
                ? AssemblyLoadContext.Default.LoadFromAssemblyPath(candidate)
                : null;
        };
    }

    [DllImport("kernel32.dll", EntryPoint = "SetDllDirectoryW", CharSet = CharSet.Unicode)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool SetDllDirectory(string path);
}
