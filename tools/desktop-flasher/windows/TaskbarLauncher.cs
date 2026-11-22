using System;
using System.Management.Automation;
using System.Management.Automation.Runspaces;
using System.IO;
using System.Reflection;
using System.Windows.Forms;

internal static class MeshCoreLauncher
{
    [STAThread]
    private static int Main()
    {
        string directory = Path.Combine(Path.GetTempPath(), "MeshCore-" + Guid.NewGuid().ToString("N"));
        try
        {
            Directory.CreateDirectory(directory);
            string script = Extract(directory, "MeshCore.ps1");
            using (var archive = new System.IO.Compression.ZipArchive(Assembly.GetExecutingAssembly().GetManifestResourceStream("engine.zip"))) System.IO.Compression.ZipFileExtensions.ExtractToDirectory(archive, Path.Combine(directory, "engine"));
            Extract(directory, "ommesh.ico");
            foreach (string name in Assembly.GetExecutingAssembly().GetManifestResourceNames())
                if (name != "engine.zip" && !File.Exists(Path.Combine(directory, name))) Extract(directory, name);
            var settings = InitialSessionState.CreateDefault();
            settings.ExecutionPolicy = Microsoft.PowerShell.ExecutionPolicy.Bypass;
            using (var runspace = RunspaceFactory.CreateRunspace(settings))
            {
                runspace.ApartmentState = System.Threading.ApartmentState.STA;
                runspace.ThreadOptions = PSThreadOptions.UseCurrentThread;
                runspace.Open();
                Runspace.DefaultRunspace = runspace;
                using (var shell = PowerShell.Create())
                {
                    shell.Runspace = runspace;
                    shell.AddCommand(script).Invoke();
                    if (shell.HadErrors)
                        throw new InvalidOperationException(shell.Streams.Error[0].ToString());
                }
                return 0;
            }
        }
        catch (Exception error)
        {
            MessageBox.Show("Не удалось запустить MeshCore. " + error.Message, "MeshCore");
            return 1;
        }
        finally
        {
            try { if (Directory.Exists(directory)) Directory.Delete(directory, true); }
            catch (IOException) { }
            catch (UnauthorizedAccessException) { }
        }
    }

    private static string Extract(string directory, string name)
    {
        string path = Path.Combine(directory, name);
        using (Stream resource = Assembly.GetExecutingAssembly().GetManifestResourceStream(name))
        using (var file = new FileStream(path, FileMode.CreateNew))
            resource.CopyTo(file);
        return path;
    }
}




