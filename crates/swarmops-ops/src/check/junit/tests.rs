//! The JUnit reader: nextest's report, the format's shapes, and every refusal.

use swarmops_api::{CheckFindingLevel, CheckReportFault};

use super::*;

/// The JUnit report nextest's `ci` profile wrote for three tests of swarmops-api, two of whose
/// assertions were broken for it (tests/fixtures/check/nextest-junit.xml).
const NEXTEST: &[u8] = include_bytes!("../../../tests/fixtures/check/nextest-junit.xml");

/// What `xml` reads as, in the root directory, or the test fails.
fn read_ok(xml: &str) -> JunitRead {
    read(xml.as_bytes(), ".").unwrap_or_else(|e| panic!("{xml}: {e:?}"))
}

/// The fault `bytes` is refused for, and its words.
fn refused(bytes: &[u8]) -> (CheckReportFault, String) {
    match read(bytes, ".") {
        Ok(read) => panic!("read: {read:?}"),
        Err(refused) => refused,
    }
}

/// This repository's nextest JUnit file with two failed cases gives two findings with their test
/// names (M31-5's acceptance): `error`, titled by the test, grouped by its binary, the message
/// the panic's first line, its assertion and the end of its stderr; three tests, two failed.
#[test]
fn nextest_s_report_gives_its_two_failed_tests() {
    let read = read(NEXTEST, ".").unwrap();
    assert_eq!((read.tests, read.failed, read.skipped), (3, 2, 0));
    let titles: Vec<&str> = read.findings.iter().map(|f| f.title.as_str()).collect();
    assert_eq!(
        titles,
        [
            "schema::check::a_check_log_request_defaults_to_the_newest_run",
            "schema::check::a_check_run_request_defaults_to_the_whole_gate",
        ]
    );
    for finding in &read.findings {
        assert_eq!(finding.level, CheckFindingLevel::Error);
        assert_eq!(finding.group.as_deref(), Some("swarmops-api::all"));
        assert_eq!((&finding.path, finding.line), (&None, None));
        assert_eq!(finding.report, 0);
    }
    let message = read.findings[1].message.as_deref().unwrap();
    assert!(
        message.starts_with(
            "thread 'schema::check::a_check_run_request_defaults_to_the_whole_gate' (43778195) \
             panicked at crates/swarmops-api/tests/schema.rs:2247:9\n\n"
        ),
        "{message}"
    );
    assert!(
        message.contains("  left: (BeforeLand, 0)\n right: (Manual, 0)"),
        "{message}"
    );
    assert!(message.contains("\n\nstage: BeforeLand\n"), "{message}");
}

/// A `testsuite` root reads; nested suites give a case without a class the nearest suite's name;
/// a class wins over the suite; a case outside any named suite has no group.
#[test]
fn a_case_s_group_is_its_class_or_its_nearest_suite() {
    let read = read_ok(
        r#"<testsuites><testsuite name="outer"><testsuite name="inner">
             <testcase name="a"><failure/></testcase></testsuite>
             <testcase name="b"><failure/></testcase>
             <testcase name="c" classname="k"><failure/></testcase></testsuite>
             <testsuite><testcase name="d"><error/></testcase></testsuite></testsuites>"#,
    );
    let groups: Vec<(&str, Option<&str>)> = (read.findings.iter())
        .map(|f| (f.title.as_str(), f.group.as_deref()))
        .collect();
    assert_eq!(
        groups,
        [
            ("a", Some("inner")),
            ("b", Some("outer")),
            ("c", Some("k")),
            ("d", None)
        ]
    );
    let single = read_ok(r#"<testsuite name="s"><testcase name="t"/></testsuite>"#);
    assert_eq!((single.tests, single.failed), (1, 0));
}

/// `error` is a failure; `skipped` counts; a failure and an error in one case are one finding;
/// a case without a name is titled so; the suites' own counts are not read.
#[test]
fn errors_fail_skips_count_and_a_case_is_one_finding() {
    let read = read_ok(
        r#"<testsuites tests="99" failures="99">
             <testsuite name="s" tests="99" failures="0">
               <testcase name="e"><error message="boom"/></testcase>
               <testcase name="s1"><skipped/></testcase>
               <testcase name="s2"><skipped message="later"></skipped></testcase>
               <testcase name="both"><failure message="one"/><error message="two"/></testcase>
               <testcase><failure/></testcase>
               <testcase name="fine"><system-err>noise</system-err></testcase>
             </testsuite></testsuites>"#,
    );
    assert_eq!((read.tests, read.failed, read.skipped), (6, 3, 2));
    let shown: Vec<(&str, Option<&str>)> = (read.findings.iter())
        .map(|f| (f.title.as_str(), f.message.as_deref()))
        .collect();
    assert_eq!(
        shown,
        [
            ("e", Some("boom")),
            ("both", Some("one")),
            ("(no name)", None)
        ]
    );
}

/// CDATA is text; XML's five entities and character references are decoded, in text and in
/// attributes; comments and processing instructions are skipped.
#[test]
fn cdata_entities_and_character_references_decode() {
    let read = read_ok(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><!-- a comment --><?pi data?>\
         <testsuite name=\"a&amp;b\"><testcase name=\"x &lt;y&gt; &#x41;&#66;\">\
         <failure message=\"&quot;q&quot; &apos;s&apos;\"><![CDATA[<raw & text>]]> &amp; more\
         </failure></testcase></testsuite>",
    );
    let finding = &read.findings[0];
    assert_eq!(finding.title, "x <y> AB");
    assert_eq!(finding.group.as_deref(), Some("a&b"));
    assert_eq!(
        finding.message.as_deref(),
        Some("\"q\" 's'\n\n<raw & text> & more")
    );
}

/// A case's `file` and `line` give the finding's path, joined to the check's directory, and its
/// line; a file that is not a repository path once joined is left out, with its line, and the
/// finding kept.
#[test]
fn a_case_s_file_and_line_give_its_path() {
    let xml = r#"<testsuite>
        <testcase name="a" file="./src/lib.rs" line="12"><failure/></testcase>
        <testcase name="b" file="../outside.rs" line="3"><failure/></testcase>
        <testcase name="c" file="/abs/x.rs"><failure/></testcase>
        <testcase name="d" file="src/x.rs" line="0"><failure/></testcase>
        </testsuite>"#;
    let read = read(xml.as_bytes(), "crates/app").unwrap();
    let shown: Vec<(Option<&str>, Option<u64>)> = (read.findings.iter())
        .map(|f| (f.path.as_deref(), f.line))
        .collect();
    assert_eq!(
        shown,
        [
            (Some("crates/app/src/lib.rs"), Some(12)),
            (None, None),
            (None, None),
            (Some("crates/app/src/x.rs"), None),
        ]
    );
    assert_eq!(read.failed, 4);
    assert_eq!(tree_path(".", "a/b.rs").as_deref(), Some("a/b.rs"));
    assert_eq!(tree_path("d", "../d/x").as_deref(), None);
}

/// The message: the failure's message, its text, then the end of the case's stderr, each when
/// not empty, joined by blank lines, within 4 KiB; the stderr cut from its front, with `…`.
#[test]
fn a_message_ends_with_the_end_of_stderr() {
    let long = format!("start{}END", "x".repeat(10_000));
    let xml = format!(
        "<testsuite><testcase name=\"a\"><failure message=\"m\">  text  </failure>\
         <system-out>not read</system-out><system-err>{long}</system-err></testcase>\
         <testcase name=\"b\"><failure>{}</failure><system-err>tail</system-err></testcase>\
         </testsuite>",
        "y".repeat(5_000)
    );
    let read = read_ok(&xml);
    let first = read.findings[0].message.as_deref().unwrap();
    assert!(first.starts_with("m\n\ntext\n\n…xxx"), "{}", &first[..20]);
    assert!(first.ends_with("xEND"), "{first:?}");
    assert!(first.len() <= MAX_FINDING_MESSAGE_BYTES, "{}", first.len());
    assert!(!first.contains("not read"));
    let second = read.findings[1].message.as_deref().unwrap();
    assert_eq!(second.len(), MAX_FINDING_MESSAGE_BYTES);
    assert!(second.ends_with("y…"), "{}", &second[second.len() - 10..]);
}

/// Each fault, refused with its words; a document type before any finding is read, whatever
/// it defines.
#[test]
fn each_fault_is_refused() {
    let deep = format!(
        "<testsuites>{}{}</testsuites>",
        "<a>".repeat(MAX_REPORT_DEPTH),
        "</a>".repeat(MAX_REPORT_DEPTH)
    );
    let fits = format!(
        "<testsuites>{}{}</testsuites>",
        "<a>".repeat(MAX_REPORT_DEPTH - 1),
        "</a>".repeat(MAX_REPORT_DEPTH - 1)
    );
    assert_eq!(read_ok(&fits).tests, 0);
    let doctype = "<?xml version=\"1.0\"?>\n<!DOCTYPE testsuites [\n<!ENTITY a \"aaaa\">\n\
                   <!ENTITY b \"&a;&a;&a;&a;\">\n]>\n<testsuites><testsuite>\
                   <testcase name=\"&b;\"><failure/></testcase></testsuite></testsuites>";
    let cases: Vec<(Vec<u8>, CheckReportFault, &str)> = vec![
        (
            doctype.as_bytes().to_vec(),
            CheckReportFault::Doctype,
            "a document type declaration, which could define entities: not read",
        ),
        (
            b"<testsuite><testcase name=\"a\"><failure>&nbsp;</failure></testcase></testsuite>"
                .to_vec(),
            CheckReportFault::Syntax,
            "an entity reference &nbsp;, which is not one of XML's five",
        ),
        (
            b"<testsuite><testcase name=\"&x;\"/></testsuite>".to_vec(),
            CheckReportFault::Syntax,
            "the attribute name holds an entity that is not one of XML's five",
        ),
        (
            b"<testsuite><x y=\"&x;\"/></testsuite>".to_vec(),
            CheckReportFault::Syntax,
            "an attribute holds an entity that is not one of XML's five",
        ),
        (
            deep.into_bytes(),
            CheckReportFault::TooDeep,
            "elements nested past 64",
        ),
        (
            [&[0xFF, 0xFE][..], "<testsuite/>".as_bytes()].concat(),
            CheckReportFault::NotText,
            "a UTF-16 byte order mark: only UTF-8 is read",
        ),
        (
            b"<testsuite name=\"\xff\"/>".to_vec(),
            CheckReportFault::NotText,
            "not UTF-8, at byte 17",
        ),
        (
            b"<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><testsuite/>".to_vec(),
            CheckReportFault::NotText,
            "declares the encoding \"ISO-8859-1\": only UTF-8 is read",
        ),
        (
            b"<report><testcase name=\"a\"/></report>".to_vec(),
            CheckReportFault::Shape,
            "a root element <report>, not <testsuites> or <testsuite>",
        ),
        (
            b"{\"version\": \"2.1.0\"}".to_vec(),
            CheckReportFault::Syntax,
            "no root element: not XML",
        ),
        (
            b"<testsuite><testcase name=\"a\"><failure>half".to_vec(),
            CheckReportFault::Syntax,
            "it ends inside an element: cut short",
        ),
        (
            b"<testsuite></testsuites>".to_vec(),
            CheckReportFault::Syntax,
            "not well-formed XML, at byte 11",
        ),
        (
            b"<testsuite/><testsuite/>".to_vec(),
            CheckReportFault::Syntax,
            "a second root element",
        ),
    ];
    for (bytes, fault, words) in cases {
        let (got, said) = refused(&bytes);
        assert_eq!(
            (got, said.as_str()),
            (fault, words),
            "{}",
            String::from_utf8_lossy(&bytes)
        );
    }
}

/// Text outside the root element is refused, before it or after it, as text, as a CDATA section
/// or as a reference; blank text there reads, and text with no root at all is still no XML.
#[test]
fn text_outside_the_root_is_refused() {
    for xml in [
        "<testsuite/>x",
        "x<testsuite/>",
        "<testsuite/><![CDATA[x]]>",
        "<testsuite/>&amp;",
    ] {
        let (fault, said) = refused(xml.as_bytes());
        assert_eq!(
            (fault, said.as_str()),
            (CheckReportFault::Syntax, "text outside the root element"),
            "{xml}"
        );
    }
    assert_eq!(read_ok("\n<testsuite/>\n").tests, 0);
    let (_, said) = refused(b"x");
    assert_eq!(said, "no root element: not XML");
}

/// A UTF-8 byte order mark and a declaration of UTF-8, in any case, read.
#[test]
fn utf_8_reads_with_its_mark_or_its_name() {
    let marked = [&b"\xEF\xBB\xBF"[..], b"<testsuite><testcase/></testsuite>"].concat();
    assert_eq!(read(&marked, ".").unwrap().tests, 1);
    let named = "<?xml version=\"1.0\" encoding=\"UTF8\"?><testsuite/>";
    assert_eq!(read_ok(named).tests, 0);
}

/// A reader asked to keep one finding keeps one and counts every failed case.
#[test]
fn a_reader_keeps_what_it_is_asked_to() {
    let read = read_keeping(NEXTEST, ".", 1).unwrap();
    assert_eq!((read.findings.len(), read.failed), (1, 2));
}

/// An entity other than XML's five is refused in any attribute of a suite or a case, as in one
/// this reader takes.
#[test]
fn an_unknown_entity_in_any_attribute_is_refused() {
    for xml in [
        "<testsuite><testcase name=\"a\" time=\"&x;\"/></testsuite>",
        "<testsuite id=\"&x;\"><testcase name=\"a\"/></testsuite>",
    ] {
        assert_eq!(
            refused(xml.as_bytes()),
            (
                CheckReportFault::Syntax,
                "an attribute holds an entity that is not one of XML's five".to_owned()
            ),
            "{xml}"
        );
    }
}
