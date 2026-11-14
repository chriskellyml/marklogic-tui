// Java source launcher (Java 11+). Run in the snapshot directory with the project's CoRB jars.
import com.marklogic.developer.corb.Manager;
import java.io.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.Properties;

class WorkbenchCorb {
    public static void main(String[] args) throws Exception {
        Path options = Path.of("job.options");
        boolean dry = Boolean.parseBoolean(args[0]);
        String mode = dry ? "dry" : "wet";
        Properties job = new Properties();
        if (dry) {
            try (Reader input = Files.newBufferedReader(Path.of(args[1]))) { job.load(input); }
        } else {
            try (InputStream input = Files.newInputStream(options)) { job.load(input); }
        }
        // CoRB accepts both underscore and hyphen spellings for its options.
        job.keySet().removeIf(key -> {
            String name = key.toString().replace('_', '-');
            return name.startsWith("XCC-") || name.startsWith("SSL-") || name.equals("MODULES-DATABASE");
        });
        job.setProperty("XCC-HOSTNAME", args[2]);
        job.setProperty("XCC-PORT", args[3]);
        job.setProperty("XCC-USERNAME", args[4]);
        job.setProperty("XCC-DBNAME", args[5]);
        job.setProperty("XCC-PROTOCOL", args[6]);
        job.setProperty("SSL-CONFIG-CLASS", "com.marklogic.developer.corb." + args[7]);
        if (!args[8].isEmpty()) job.setProperty("MODULES-DATABASE", args[8]);
        for (String prefix : new String[]{"URIS-MODULE.", "PROCESS-MODULE."}) {
            job.setProperty(prefix + "DRY-RUN", args[0]);
            job.setProperty(prefix + "LIMIT", args[9]);
            job.setProperty(prefix + "CORB-PROJECT", args[10]);
            job.setProperty(prefix + "JOB-NAME", args[11]);
            job.setProperty(prefix + "RUN-ID", args[12]);
            job.setProperty(prefix + "USER-ID", System.getProperty("user.name"));
        }
        job.setProperty("PROCESS-MODULE.DELIM", job.getProperty("BATCH-URI-DELIM", ";"));
        job.putIfAbsent("PROCESS-TASK", "com.marklogic.developer.corb.ExportBatchToFileTask");
        job.setProperty("EXPORT-FILE-NAME", mode + "-report.txt");
        Path effective = dry ? options : Path.of("wet.options");
        try (OutputStream out = Files.newOutputStream(effective)) { job.store(out, "MarkLogic Workbench"); }
        System.setProperty("OPTIONS-FILE", effective.toString());
        System.setProperty("XCC-PASSWORD", new String(System.in.readAllBytes(), StandardCharsets.UTF_8));
        Manager.main(new String[0]);
    }
}
