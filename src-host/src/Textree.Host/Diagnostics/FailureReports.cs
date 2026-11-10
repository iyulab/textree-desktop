using System.Reflection;
using TauriKit.Diagnostics;
using Textree.Host.Rag;

namespace Textree.Host.Diagnostics;

/// <summary>Where the host says that something failed. Never blocks, never throws.</summary>
public interface IFailureReports
{
    void ReportError(string eventName, string? modelSlot, ModelPhase phase, Exception ex);
}

/// <summary>Used when the shell named no reports folder (tests, the host started by hand).</summary>
public sealed class NoFailureReports : IFailureReports
{
    public void ReportError(string eventName, string? modelSlot, ModelPhase phase, Exception ex) { }
}

/// <summary>
/// The host's failures, written as reports of the layer <c>textree-host</c> into the folder the shell named — one file
/// of this process's own (<see cref="Segment"/>) in the same stream as the shell's reports. The shell verifies each line
/// against the same registry and sends what verifies when telemetry is on; nothing here talks to the network.
///
/// What a report carries is the kit's allowlist: the exception's type name, the method names of the stack (only this
/// host's own — <see cref="OwnFrames"/>), and the details <c>registry.json</c> declares for the layer. The exception's
/// message is never read.
/// </summary>
public sealed class FailureReports : IFailureReports, IDisposable
{
    /// <summary>The variable the shell names the reports folder in. The host never picks one itself.</summary>
    public const string ReportsDirVar = "TEXTREE_REPORTS_DIR";

    private const string LayerName = "textree-host";
    private const string OwnNamespace = "Textree.Host.";

    private readonly Segment _segment;
    private readonly Reporter _reporter;
    private readonly Layer _layer;
    private readonly string _version;
    private readonly (string, string)[] _machine;

    private FailureReports(Segment segment, Registry registry, string version)
    {
        _segment = segment;
        _reporter = new Reporter(segment.File);
        _layer = registry.Layer(LayerName);
        _version = version;
        _machine =
        [
            ("cpu_count", Environment.ProcessorCount.ToString(System.Globalization.CultureInfo.InvariantCulture)),
            ("ram_gb", (GC.GetGCMemoryInfo().TotalAvailableMemoryBytes / (1024L * 1024 * 1024))
                .ToString(System.Globalization.CultureInfo.InvariantCulture)),
        ];
    }

    /// <summary>Reports into <paramref name="dir"/>, or nowhere when it is not given.</summary>
    public static IFailureReports Open(string? dir)
    {
        if (string.IsNullOrWhiteSpace(dir)) return new NoFailureReports();
        try
        {
            return new FailureReports(Segment.Open(dir), EmbeddedRegistry(), Version());
        }
        catch (Exception)
        {
            return new NoFailureReports(); // a folder that cannot be written must not stop the host
        }
    }

    public void ReportError(string eventName, string? modelSlot, ModelPhase phase, Exception ex)
    {
        try
        {
            var details = new List<(string, string)>(_machine)
            {
                ("event", eventName),
                ("phase", phase.ToString()),
            };
            if (modelSlot is not null) details.Add(("model_slot", modelSlot));
            var kind = ex.GetType().FullName ?? Report.UnrecognizedKind;
            _reporter.Record(_layer.Report(kind, OwnFrames(ex.StackTrace), _version, [.. details]));
        }
        catch (Exception)
        {
            // Reporting a failure must never become one.
        }
    }

    /// <summary>
    /// The lines of <paramref name="stack"/> that are this host's own code. A framework's or a library's frames say how
    /// it got there, not where in this host it failed, and are left out.
    /// </summary>
    public static string OwnFrames(string? stack) =>
        string.Join('\n', (stack ?? "").Split('\n').Where(line =>
        {
            var trimmed = line.TrimStart();
            if (trimmed.StartsWith("at ", StringComparison.Ordinal)) trimmed = trimmed[3..];
            if (trimmed.StartsWith("async ", StringComparison.Ordinal)) trimmed = trimmed[6..];
            return trimmed.StartsWith(OwnNamespace, StringComparison.Ordinal);
        }));

    /// <summary>The registry the shell verifies with — the same file, compiled into both.</summary>
    public static Registry EmbeddedRegistry()
    {
        using var stream = typeof(FailureReports).Assembly.GetManifestResourceStream("textree-registry.json")
            ?? throw new InvalidOperationException("the reports registry is not embedded");
        using var reader = new StreamReader(stream);
        return Registry.FromJson(reader.ReadToEnd());
    }

    /// <summary>The host's version, without build metadata.</summary>
    public static string Version()
    {
        var version = typeof(FailureReports).Assembly
            .GetCustomAttribute<AssemblyInformationalVersionAttribute>()?.InformationalVersion
            ?? typeof(FailureReports).Assembly.GetName().Version?.ToString()
            ?? "0.0.0";
        var plus = version.IndexOf('+');
        return plus >= 0 ? version[..plus] : version;
    }

    public void Dispose() => _segment.Dispose();
}

/// <summary>The failures the host reports, by name — the registry's <c>event</c> choice.</summary>
public static class HostEvent
{
    public const string ModelDownloadFailed = "model.download.failed";
    public const string ModelLoadFailed = "model.load.failed";
    public const string EmbedderInitFailed = "embedder.init.failed";
    public const string IndexFailed = "index.failed";
    public const string ReindexFailed = "reindex.failed";
    public const string GenerationPrepareFailed = "generation.prepare.failed";

    /// <summary>Which event a model failure is, by the phase the model was in when it failed.</summary>
    public static string ForModelFailure(ModelPhase phaseAtFailure) => phaseAtFailure switch
    {
        ModelPhase.Downloading => ModelDownloadFailed,
        ModelPhase.Loading => ModelLoadFailed,
        _ => EmbedderInitFailed,
    };
}
