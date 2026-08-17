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

#[cfg(unix)]
#[test]
fn recursive_remove_removes_a_dangling_symlink() {
    let root = sandbox("dangling-recursive-remove");
    let link = root.join("stale-link");
    std::os::unix::fs::symlink(root.join("missing-target"), &link).unwrap();

    shrimp::Script::parse("remove --recursive --force \"stale-link\"\n")
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();

    assert!(matches!(
        std::fs::symlink_metadata(&link),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    ));
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
fn nested_parallel_for_does_not_multiply_the_outer_limit() {
    let started = std::time::Instant::now();
    let source = r#"
        parallel for outer in words "one two" limit 2
          parallel for inner in words "left right" limit 2
            $ sh -c "sleep .1"
          end
        end
    "#;
    let report = shrimp::Script::parse(source)
        .unwrap()
        .run(&Context::default())
        .unwrap();

    assert_eq!(report.commands_run, 4);
    assert!(started.elapsed() >= Duration::from_millis(180));
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

#[test]
fn typed_values_compare_index_and_iterate_without_word_splitting() {
    let root = sandbox("typed-values");
    let source = r#"
        let ready = true
        let attempts = 4
        let items = lines "alpha\ntwo words\ngamma"
        if ready and attempts > 3
          write "nested/result" <- "${items[1]}"
        end
        for item in ${items}
          append "nested/all" <- "${item}\n"
        end
    "#;
    shrimp::Script::parse(source)
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("nested/result")).unwrap(),
        "two words"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("nested/all")).unwrap(),
        "alpha\ntwo words\ngamma\n"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn structured_values_cannot_be_interpolated_ambiguously() {
    let error = shrimp::Script::parse("let values = words \"a b\"\nprint \"${values}\"\n")
        .unwrap()
        .run(&Context::default())
        .unwrap_err();
    assert!(
        error.to_string().contains("list cannot be interpolated"),
        "{error}"
    );
    assert!(error.to_string().contains("script line 2"), "{error}");
}

#[test]
fn exists_and_integer_type_errors_are_explicit() {
    let root = sandbox("typed-conditions");
    std::fs::write(root.join("present"), "yes").unwrap();
    shrimp::Script::parse("if exists \"present\"\n  write \"ok\" <- yes\nend\n")
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();
    assert!(root.join("ok").exists());
    let error = shrimp::Script::parse("if name > 2\n  print no\nend\n")
        .unwrap()
        .run(&Context::new(&root))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("ordered comparisons require integers"),
        "{error}"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn rust_pipeline_accepts_explicit_stdin_bytes() {
    let output = cmd("cat")
        .pipeline()
        .stdin("from-memory")
        .run(&Context::default())
        .unwrap();
    assert_eq!(output.stdout, b"from-memory");
}

#[test]
fn includes_reusable_functions_relative_to_each_including_file_once() {
    let root = sandbox("includes");
    std::fs::create_dir_all(root.join("lib/nested")).unwrap();
    std::fs::write(
        root.join("lib/common.shrimp"),
        "include \"nested/message.shrimp\"\nappend \"loaded\" <- x\nfn publish value\n  write \"result\" <- \"${prefix}:${value}\"\nend\n",
    ).unwrap();
    std::fs::write(
        root.join("lib/nested/message.shrimp"),
        "let prefix = reusable\n",
    )
    .unwrap();
    let script = shrimp::Script::parse(
        "include \"lib/common.shrimp\"\ninclude \"lib/common.shrimp\"\ncall publish artifact\n",
    )
    .unwrap();
    script.run(&Context::new(&root)).unwrap();
    assert_eq!(std::fs::read_to_string(root.join("loaded")).unwrap(), "x");
    assert_eq!(
        std::fs::read_to_string(root.join("result")).unwrap(),
        "reusable:artifact"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn include_cycles_report_the_file_and_call_site() {
    let root = sandbox("include-cycle");
    std::fs::write(root.join("a.shrimp"), "include \"b.shrimp\"\n").unwrap();
    std::fs::write(root.join("b.shrimp"), "include \"a.shrimp\"\n").unwrap();
    let error = shrimp::Script::parse("include \"a.shrimp\"\n")
        .unwrap()
        .run(&Context::new(&root))
        .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("include cycle detected"), "{message}");
    assert!(message.contains("a.shrimp"), "{message}");
    assert!(message.contains("script line 1"), "{message}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn functions_implicitly_return_their_last_typed_value() {
    let root = sandbox("function-values");
    let source = r#"
        fn identity input
          value ${input}
        end
        fn attempts
          let count = 4
        end
        let original = lines "one\ntwo words"
        call returned <- identity ${original}
        call count <- attempts
        if count == 4
          write "result" <- "${returned[1]}"
        end
    "#;
    shrimp::Script::parse(source)
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("result")).unwrap(),
        "two words"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn script_commands_accept_file_and_value_stdin_and_environment_overrides() {
    let root = sandbox("command-input-env");
    std::fs::write(root.join("input"), "from-file").unwrap();
    let source = r#"
        $ sh -c "cat > file-result" < "input"
        $ cat < "input" > "combined-result"
        let input = "from-value"
        env OUTPUT=value-result $ sh -c "cat > \"$OUTPUT\"" <<< "${input}"
        $ sh -c "printf ignored; printf diagnostic >&2" > discard
        $ sh -c "printf visible; printf ignored >&2" 2> discard
    "#;
    shrimp::Script::parse(source)
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("file-result")).unwrap(),
        "from-file"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("value-result")).unwrap(),
        "from-value"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("combined-result")).unwrap(),
        "from-file"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn temporary_paths_are_unique_typed_and_cleaned_after_execution() {
    let root = sandbox("temporary-paths");
    let source = r#"
        temp_file scratch
        write "temp-path" <- "${scratch}"
        temp_dir staging
        write "${staging}/nested/value" <- ok
        write "dir-path" <- "${staging}"
    "#;
    shrimp::Script::parse(source)
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();
    let file = std::fs::read_to_string(root.join("temp-path")).unwrap();
    let directory = std::fs::read_to_string(root.join("dir-path")).unwrap();
    assert_ne!(file, directory);
    assert!(!std::path::Path::new(&file).exists());
    assert!(!std::path::Path::new(&directory).exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn temporary_paths_are_cleaned_when_the_workflow_fails() {
    let root = sandbox("temporary-error-cleanup");
    let workflow = shrimp::Script::parse(
        "temp_dir staging\nwrite \"remembered\" <- \"${staging}\"\n$ shrimp-command-that-does-not-exist\n",
    ).unwrap();
    assert!(workflow.run(&Context::new(&root)).is_err());
    let path = std::fs::read_to_string(root.join("remembered")).unwrap();
    assert!(!std::path::Path::new(&path).exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn file_metadata_produces_integer_values() {
    let root = sandbox("metadata");
    std::fs::write(root.join("artifact"), "12345").unwrap();
    let source = r#"
        file_size bytes <- "artifact"
        modified_time changed <- "artifact"
        if bytes == 5 and changed > 0
          write "result" <- "${bytes}"
        end
    "#;
    shrimp::Script::parse(source)
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();
    assert_eq!(std::fs::read_to_string(root.join("result")).unwrap(), "5");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn typed_conditions_preserve_quotes_whitespace_and_nested_lookup() {
    let root = sandbox("condition-token-metadata");
    let source = r#"
        let ready = true
        let name = "two words"
        record row tsv "10" fields size
        if "4" != 4 and "true" != true and name == "two words" and row.size == "10" and ready
          write "result" <- correct
        else
          write "result" <- wrong
        end
    "#;
    shrimp::Script::parse(source)
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("result")).unwrap(),
        "correct"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[cfg(unix)]
#[test]
fn command_conditions_ignore_quoted_operators_and_output_redirects() {
    let root = sandbox("command-condition-classification");
    let source = r#"
        if printf "a and b"
          write "quoted" <- yes
        end
        if sh -c "exit 0" > "condition-output"
          write "redirected" <- yes
        end
    "#;
    shrimp::Script::parse(source)
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();
    assert!(root.join("quoted").exists());
    assert!(root.join("redirected").exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn functions_retain_their_include_directory_for_deferred_includes() {
    let root = sandbox("deferred-include-directory");
    std::fs::create_dir_all(root.join("lib/nested")).unwrap();
    std::fs::write(
        root.join("lib/functions.shrimp"),
        "fn load\n  include \"nested/value.shrimp\"\nend\n",
    )
    .unwrap();
    std::fs::write(
        root.join("lib/nested/value.shrimp"),
        "write \"loaded\" <- yes\n",
    )
    .unwrap();
    shrimp::Script::parse("include \"lib/functions.shrimp\"\ncall load\n")
        .unwrap()
        .run(&Context::new(&root))
        .unwrap();
    assert_eq!(std::fs::read_to_string(root.join("loaded")).unwrap(), "yes");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn parallel_branches_share_include_once_state() {
    let root = sandbox("parallel-include-once");
    std::fs::write(
        root.join("setup.shrimp"),
        "append \"loaded\" <- x\nlet prefix = shared\nfn mark name\n  write \"${prefix}-${name}\" <- yes\nend\n",
    )
    .unwrap();
    shrimp::Script::parse(
        "parallel for item in words \"left right\" limit 2\n  include \"setup.shrimp\"\n  call mark \"${item}\"\nend\n",
    )
    .unwrap()
    .run(&Context::new(&root))
    .unwrap();
    assert_eq!(std::fs::read_to_string(root.join("loaded")).unwrap(), "x");
    assert!(root.join("shared-left").exists());
    assert!(root.join("shared-right").exists());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn malformed_double_less_than_redirection_is_rejected() {
    let error = shrimp::Script::parse("$ tool << input\n")
        .unwrap()
        .run(&Context::default())
        .unwrap_err();
    assert!(
        error.to_string().contains("`< FILE` or `<<< VALUE`"),
        "{error}"
    );
    assert!(error.to_string().contains("script line 1"), "{error}");
}

#[cfg(target_os = "linux")]
#[test]
fn managed_temporary_paths_have_private_permissions() {
    let root = sandbox("temporary-permissions");
    let workflow = shrimp::Script::parse(
        "temp_file file\ntemp_dir directory\n$ sh -c \"stat -c %a $1 > file-mode\" _ \"${file}\"\n$ sh -c \"stat -c %a $1 > dir-mode\" _ \"${directory}\"\n",
    ).unwrap();
    workflow.run(&Context::new(&root)).unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("file-mode")).unwrap(),
        "600\n"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("dir-mode")).unwrap(),
        "700\n"
    );
    std::fs::remove_dir_all(root).unwrap();
}
