using System.Text.Json;
using Microsoft.Extensions.DependencyInjection;
using Textree.Host.Diagnostics;
using Textree.Host.Rag;
using Textree.Host.Tests.Fixtures;
using Xunit;

public class FailureReportsTests : IDisposable
{
    private readonly string _dir = Path.Combine(Path.GetTempPath(), "textree-reports-" + Guid.NewGuid().ToString("n"));

    public void Dispose()
    {
        try { Directory.Delete(_dir, recursive: true); } catch (IOException) { }
    }

    private string[] Lines() => Directory.Exists(_dir)
        ? Directory.GetFiles(_dir, "*.jsonl").SelectMany(File.ReadAllLines).ToArray()
        : [];

    [Fact]
    public void Without_a_folder_nothing_is_written()
    {
        Assert.IsType<NoFailureReports>(FailureReports.Open(null));
        Assert.IsType<NoFailureReports>(FailureReports.Open("  "));
    }

    [Fact]
    public void A_failure_is_one_line_of_the_host_layer_with_the_declared_details_only()
    {
        var reports = FailureReports.Open(_dir);
        // public-text: allow — a path in the exception's message is what this test keeps out of the report
        reports.ReportError(HostEvent.ModelLoadFailed, "embedder", ModelPhase.Loading, Thrower.Catch("C:/Users/someone/secret note.md"));

        var line = Assert.Single(Lines());
        Assert.DoesNotContain("secret", line);
        Assert.DoesNotContain("someone", line);
        using var json = JsonDocument.Parse(line);
        var root = json.RootElement;
        Assert.Equal("textree-host", root.GetProperty("layer").GetString());
        Assert.Equal("System.IO.FileNotFoundException", root.GetProperty("kind").GetString());
        var details = root.GetProperty("details");
        Assert.Equal("model.load.failed", details.GetProperty("event").GetString());
        Assert.Equal("Loading", details.GetProperty("phase").GetString());
        Assert.Equal("embedder", details.GetProperty("model_slot").GetString());
        Assert.True(details.TryGetProperty("cpu_count", out _));
        Assert.True(details.TryGetProperty("ram_gb", out _));
        ((IDisposable)reports).Dispose();
    }

    [Fact]
    public void Only_the_hosts_own_frames_are_kept()
    {
        var reports = FailureReports.Open(_dir);
        reports.ReportError(HostEvent.IndexFailed, "embedder", ModelPhase.Error, Thrower.Catch("x"));
        using var json = JsonDocument.Parse(Assert.Single(Lines()));
        var frames = json.RootElement.GetProperty("frames").EnumerateArray().Select(f => f.GetString()!).ToList();
        Assert.NotEmpty(frames);
        Assert.All(frames, f => Assert.StartsWith("Textree.Host.", f));
        ((IDisposable)reports).Dispose();
    }

    [Fact]
    public void An_event_the_registry_does_not_declare_is_left_out_of_the_details()
    {
        var reports = FailureReports.Open(_dir);
        reports.ReportError("something.else", null, ModelPhase.Error, Thrower.Catch("x"));
        using var json = JsonDocument.Parse(Assert.Single(Lines()));
        Assert.False(json.RootElement.GetProperty("details").TryGetProperty("event", out _));
        ((IDisposable)reports).Dispose();
    }

    [Fact]
    public void The_same_failure_twice_is_written_once()
    {
        var reports = FailureReports.Open(_dir);
        for (var i = 0; i < 2; i++)
            reports.ReportError(HostEvent.ReindexFailed, "embedder", ModelPhase.Error, Thrower.Catch("x"));
        Assert.Single(Lines());
        ((IDisposable)reports).Dispose();
    }

    [Fact]
    public void The_embedded_registry_declares_the_host_layer_and_every_event()
    {
        var registry = FailureReports.EmbeddedRegistry();
        Assert.True(registry.Reports.ContainsKey("textree-host"));
        var declared = registry.Reports["textree-host"]["event"];
        foreach (var name in typeof(HostEvent).GetFields().Where(f => f.IsLiteral).Select(f => (string)f.GetValue(null)!))
            Assert.Contains(name, ((TauriKit.Diagnostics.FieldKind.Choice)declared).Values);
    }

    /// <summary>
    /// The line the shell's golden test verifies against its registry (src-tauri/src/telemetry/host-report.golden.jsonl)
    /// is the line this host writes — apart from what depends on the machine and the moment.
    /// </summary>
    [Fact]
    public void A_report_matches_the_line_the_shell_verifies()
    {
        var reports = FailureReports.Open(_dir);
        reports.ReportError(HostEvent.ModelDownloadFailed, "generator", ModelPhase.Downloading, Thrower.Catch("x"));
        var written = Normalized(Assert.Single(Lines()));
        var golden = Normalized(File.ReadAllLines(GoldenPath()).Single(l => l.Length > 0));
        Assert.Equal(golden, written);
        ((IDisposable)reports).Dispose();
    }

    [Fact]
    public void The_host_resolves_no_reports_when_the_shell_names_no_folder()
    {
        using var factory = new HostFactory();
        using var scope = factory.Services.CreateScope();
        Assert.IsType<NoFailureReports>(scope.ServiceProvider.GetRequiredService<IFailureReports>());
    }

    private static string Normalized(string line)
    {
        using var json = JsonDocument.Parse(line);
        var root = json.RootElement;
        var details = root.GetProperty("details").EnumerateObject()
            .Where(p => p.Name is not ("cpu_count" or "ram_gb"))
            .ToDictionary(p => p.Name, p => p.Value.GetString());
        return JsonSerializer.Serialize(new
        {
            layer = root.GetProperty("layer").GetString(),
            kind = root.GetProperty("kind").GetString(),
            frames = root.GetProperty("frames").EnumerateArray().Select(f => f.GetString()).ToArray(),
            details,
        });
    }

    private static string GoldenPath([System.Runtime.CompilerServices.CallerFilePath] string here = "") =>
        Path.GetFullPath(Path.Combine(Path.GetDirectoryName(here)!, "../../../../src-tauri/src/telemetry/host-report.golden.jsonl"));
}

namespace Textree.Host.Tests.Fixtures
{
    /// <summary>A failure thrown from a frame of the host's own namespace.</summary>
    public static class Thrower
    {
        public static Exception Catch(string path)
        {
            try { Fail(path); }
            catch (Exception e) { return e; }
            throw new InvalidOperationException("unreachable");
        }

        [System.Runtime.CompilerServices.MethodImpl(System.Runtime.CompilerServices.MethodImplOptions.NoInlining)]
        private static void Fail(string path) => throw new FileNotFoundException($"cannot read {path}", path);
    }
}
