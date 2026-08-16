use shrimp::{Context, Error, Task, cmd, files};
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

fn sandbox(name: &str) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(format!("shrimp-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    std::fs::create_dir_all(&path).unwrap();
    path
}

#[test]
fn composes_filesystem_tasks_and_atomic_writes() {
    let root = sandbox("files");
    let context = Context::new(&root);
    let workflow = files::write("input.txt", "hello")
        .and_then(|_| files::read_to_string("input.txt"))
        .map(|text| text.to_uppercase())
        .and_then(|text| files::write_atomic("output.txt", text));

    workflow.run(&context).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("output.txt")).unwrap(),
        "HELLO"
    );
    assert_eq!(std::fs::read_dir(&root).unwrap().count(), 2);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn retries_a_lazy_task_until_it_succeeds() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&attempts);
    let task = Task::new(move |_| {
        let attempt = observed.fetch_add(1, Ordering::SeqCst);
        if attempt < 2 {
            Err(Error::message("not yet"))
        } else {
            Ok("done")
        }
    })
    .retry(3, Duration::ZERO);

    assert_eq!(task.run(&Context::default()).unwrap(), "done");
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[test]
fn reports_timeouts() {
    let task = Task::new(|_| {
        std::thread::sleep(Duration::from_millis(50));
        Ok(())
    })
    .timeout(Duration::from_millis(1));
    assert!(matches!(
        task.run(&Context::default()),
        Err(Error::Timeout { .. })
    ));
}

#[cfg(unix)]
#[test]
fn streams_between_processes_and_captures_output() {
    let output = cmd("printf")
        .args(["%s", "one\ntwo\nthree\n"])
        .pipe(cmd("wc").arg("-l"))
        .run(&Context::default())
        .unwrap();
    assert_eq!(output.stdout_string().unwrap().trim(), "3");
}

#[cfg(unix)]
#[test]
fn returns_a_structured_error_for_failed_commands() {
    let error = cmd("sh")
        .args(["-c", "printf bad-news >&2; exit 7"])
        .run(&Context::default())
        .unwrap_err();
    match error {
        Error::CommandFailed { status, stderr, .. } => {
            assert_eq!(status.code(), Some(7));
            assert_eq!(stderr, b"bad-news");
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[cfg(unix)]
#[test]
fn executes_the_workflow_language_end_to_end() {
    let root = sandbox("language");
    let source = r#"
        let out = "build output"
        mkdir "${out}"
        capture message <- printf "hello world\n"
        $ printf "%s" "${message}" | tr a-z A-Z
        write "${out}/result.txt" <- "${message}\n"
        if test -f "${out}/result.txt"
          append "${out}/result.txt" <- "exists\n"
        else
          write "${out}/result.txt" <- "missing\n"
        end
    "#;
    let script = shrimp::Script::parse(source).unwrap();
    let report = script.run(&Context::new(&root)).unwrap();

    assert_eq!(report.commands_run, 3);
    assert_eq!(report.files_changed, 3);
    assert_eq!(
        std::fs::read_to_string(root.join("build output/result.txt")).unwrap(),
        "hello world\nexists\n"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn language_errors_include_source_lines() {
    let error = shrimp::Script::parse("let ok = yes\nthis is not valid\n").unwrap_err();
    assert!(error.to_string().contains("script line 2"), "{error}");
}

#[cfg(unix)]
#[test]
fn supports_loops_globs_functions_parallelism_and_redirection() {
    let root = sandbox("language-basics");
    std::fs::create_dir(root.join("inputs")).unwrap();
    std::fs::write(root.join("inputs/b.txt"), "b").unwrap();
    std::fs::write(root.join("inputs/a.txt"), "a").unwrap();
    let source = r#"
        fn save path value
          write "${path}" <- "${value}"
        end

        parallel
          call save "left.txt" "left"
          call save "right.txt" "right"
        end

        for file in glob "inputs/*.txt"
          append "manifest.txt" <- "${file}\n"
        end

        for word in words "one two"
          append "words.txt" <- "${word}\n"
        end

        $ printf "redirected" > "stdout.txt"
        $ sh -c "printf diagnostic >&2" 2> "stderr.txt"
    "#;
    let report = shrimp::Script::parse(source)
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(root.join("left.txt")).unwrap(),
        "left"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("right.txt")).unwrap(),
        "right"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("manifest.txt")).unwrap(),
        "inputs/a.txt\ninputs/b.txt\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("stdout.txt")).unwrap(),
        "redirected"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("words.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("stderr.txt")).unwrap(),
        "diagnostic"
    );
    assert_eq!(report.commands_run, 2);
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn command_timeout_stops_a_workflow() {
    let started = std::time::Instant::now();
    let error = shrimp::Script::parse("timeout 20ms $ sh -c \"sleep 2\"\n")
        .unwrap()
        .run(&Context::default())
        .unwrap_err();
    assert!(error.to_string().contains("exceeded timeout"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn dry_run_does_not_change_files() {
    let root = sandbox("dry-run");
    let options = shrimp::ScriptOptions {
        dry_run: true,
        trace: false,
    };
    let report = shrimp::Script::parse("mkdir never-created\nwrite file <- contents\n")
        .unwrap()
        .run_with_options(&Context::new(&root), options)
        .unwrap();
    assert_eq!(report, shrimp::ScriptReport::default());
    assert!(!root.join("never-created").exists());
    assert!(!root.join("file").exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn trace_redacts_secret_values() {
    let root = sandbox("secrets");
    let script = root.join("secret.shrimp");
    std::fs::write(
        &script,
        "secret token = top-secret-value\n$ printf %s ${token}\n",
    )
    .unwrap();
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_shrimp"))
        .args(["--trace", script.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(output.status.success());
    let trace = String::from_utf8(output.stderr).unwrap();
    assert!(trace.contains("[REDACTED]"), "{trace}");
    assert!(!trace.contains("top-secret-value"), "{trace}");
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn timeout_applies_while_descendants_hold_output_pipes() {
    let started = std::time::Instant::now();
    let error = shrimp::Script::parse("timeout 20ms $ sh -c \"sleep 2 &\"\n")
        .unwrap()
        .run(&Context::default())
        .unwrap_err();
    assert!(error.to_string().contains("exceeded timeout"), "{error}");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[cfg(unix)]
#[test]
fn parallel_waits_for_all_branches_before_reporting_failure() {
    let root = sandbox("parallel-failure");
    let script = shrimp::Script::parse(
        "parallel\n  $ sh -c \"exit 9\"\n  $ sh -c \"sleep .1; touch completed\"\nend\n",
    )
    .unwrap();
    assert!(script.run(&Context::new(&root)).is_err());
    assert!(root.join("completed").exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn dry_run_parallel_branches_keep_isolated_variables() {
    let script = shrimp::Script::parse(
        "parallel\n  let branch_only = value\n  print \"${branch_only}\"\nend\n",
    )
    .unwrap();
    let error = script
        .run_with_options(
            &Context::default(),
            shrimp::ScriptOptions {
                dry_run: true,
                trace: false,
            },
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("undefined variable `branch_only`")
    );
}

#[cfg(unix)]
#[test]
fn redirection_scanner_accepts_utf8_before_operator() {
    let root = sandbox("utf8-redirection");
    shrimp::Script::parse("$ printf café > output.txt\n")
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("output.txt")).unwrap(),
        "café"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn file_operators_inside_quotes_are_not_delimiters() {
    let root = sandbox("quoted-operators");
    let script = shrimp::Script::parse(
        "write \"result<-old\" <- first\ncopy \"result<-old\" -> \"copy->name\"\nappend \"copy->name\" <- second\n",
    )
    .unwrap();
    script.run(&Context::new(&root)).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("result<-old")).unwrap(),
        "first"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("copy->name")).unwrap(),
        "firstsecond"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn atomic_write_replaces_an_existing_file() {
    let root = sandbox("atomic-replace");
    let context = Context::new(&root);
    files::write_atomic("value", "first").run(&context).unwrap();
    files::write_atomic("value", "second")
        .run(&context)
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("value")).unwrap(),
        "second"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn spawn_failure_terminates_already_started_pipeline_children() {
    let root = sandbox("spawn-cleanup");
    let marker = root.join("should-not-exist");
    let command = format!(
        "$ sh -c \"i=0; while [ \\$i -lt 200000 ]; do i=\\$((i+1)); done; touch {}\" | shrimp-definitely-missing-command\n",
        marker.display()
    );
    assert!(
        shrimp::Script::parse(&command)
            .unwrap()
            .run(&Context::new(&root))
            .is_err()
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(!marker.exists());
    std::fs::remove_dir_all(root).unwrap();
}
