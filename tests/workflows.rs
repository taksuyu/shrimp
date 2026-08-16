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
    assert_eq!(report.files_changed, 8);
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

#[test]
fn records_match_fields_and_recursive_remove_cleans_trees() {
    let root = sandbox("records-match-remove");
    std::fs::create_dir_all(root.join("stale/nested")).unwrap();
    std::fs::write(root.join("stale/nested/file"), "old").unwrap();
    let source = r#"
        record document tsv "guide\tpublished\tdocs/guide.md" fields name state path
        match "${document.state}"
        case published
          write "result.txt" <- "${document.name}:${document.path}"
        case draft
          write "result.txt" <- "draft"
        else
          write "result.txt" <- "unknown"
        end
        remove --recursive --force "stale"
        remove --recursive --force "already-absent"
    "#;
    let report = shrimp::Script::parse(source)
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(root.join("result.txt")).unwrap(),
        "guide:docs/guide.md"
    );
    assert!(!root.join("stale").exists());
    assert_eq!(report.files_changed, 2);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn record_field_count_errors_are_line_aware() {
    let error = shrimp::Script::parse("record row tsv \"one\\ttwo\" fields first second third\n")
        .unwrap()
        .run(&Context::default())
        .unwrap_err();
    assert!(error.to_string().contains("script line 1"), "{error}");
    assert!(error.to_string().contains("expects 3"), "{error}");
}

#[test]
fn recursive_remove_rejects_an_empty_path() {
    let root = sandbox("empty-recursive-remove");
    let marker = root.join("must-remain");
    std::fs::write(&marker, "safe").unwrap();
    let error = shrimp::Script::parse("remove --recursive --force \"\"\n")
        .unwrap()
        .run(&Context::new(&root))
        .unwrap_err();

    assert!(error.to_string().contains("remove path must not be empty"));
    assert!(marker.exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn bounded_parallel_for_runs_every_iteration() {
    let root = sandbox("parallel-for");
    let source = r#"
        mkdir "output"
        parallel for item in words "one two three four" limit 2
          write "output/${item}" <- "${item}"
        end
    "#;
    let report = shrimp::Script::parse(source)
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();
    for item in ["one", "two", "three", "four"] {
        assert_eq!(
            std::fs::read_to_string(root.join("output").join(item)).unwrap(),
            item
        );
    }
    assert_eq!(report.files_changed, 5);
    std::fs::remove_dir_all(root).unwrap();
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
        "$ sh -c \"sleep .1; touch {}\" | shrimp-definitely-missing-command\n",
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

#[cfg(unix)]
#[test]
fn timed_pipeline_does_not_attribute_final_stderr_to_an_intermediate_failure() {
    let error = cmd("sh")
        .args(["-c", "printf intermediate-error >&2; exit 7"])
        .pipe(cmd("sh").args(["-c", "cat; printf final-error >&2"]))
        .run_timeout(&Context::default(), Duration::from_secs(1))
        .unwrap_err();
    match error {
        Error::CommandFailed { status, stderr, .. } => {
            assert_eq!(status.code(), Some(7));
            assert!(stderr.is_empty(), "{stderr:?}");
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn interpolation_does_not_implicitly_read_the_process_environment() {
    let error = shrimp::Script::parse("print \"${PATH}\"\n")
        .unwrap()
        .run(&Context::new("."))
        .unwrap_err();
    assert!(error.to_string().contains("undefined variable `PATH`"));
}

#[test]
fn recursive_functions_stop_at_the_call_depth_limit() {
    let script = shrimp::Script::parse("fn recurse\n  call recurse\nend\ncall recurse\n").unwrap();
    let error = script.run(&Context::default()).unwrap_err();
    assert!(error.to_string().contains("function call depth exceeded"));
}

#[test]
fn worker_panic_is_not_reported_as_a_timeout() {
    let task =
        Task::<()>::new(|_| panic!("intentional worker panic")).timeout(Duration::from_secs(1));
    let error = task.run(&Context::default()).unwrap_err();
    assert!(!matches!(error, Error::Timeout { .. }));
    assert!(error.to_string().contains("worker stopped"));
}
