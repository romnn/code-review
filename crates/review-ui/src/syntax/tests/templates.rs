//! Templates: which language a template's literal text takes, and how both the template's
//! syntax and its text are highlighted, in either tier.

use super::*;

/// The template syntax of `name` whose data is the tree-house-tier language `data`.
fn template(name: &'static str, data: &'static str) -> Syntax {
    Syntax::Template {
        name,
        data: Data::TreeHouse(data),
    }
}

/// The template syntax of `name` whose data is the bat syntax `data`.
fn template_bat(name: &'static str, data: &str) -> Syntax {
    Syntax::Template {
        name,
        data: Data::Bat(data.to_owned()),
    }
}

/// The template syntax of `name` whose data is plain.
fn template_plain(name: &'static str) -> Syntax {
    Syntax::Template {
        name,
        data: Data::Plain,
    }
}

/// A template's data takes the language of the file it produces: by the path without the
/// template's extension, by the path itself for Helm's globs, by chezmoi's names, or by the
/// first line, and else the template language's own default.
#[test]
fn templates_resolve_their_data() {
    let bash = indoc! {"
        #!/bin/bash
        echo hi
    "};
    let cases: [(&str, &str, Syntax); 24] = [
        // Helm, by its globs, its text YAML unless its name says otherwise
        (
            "deploy/chart/templates/deployment.yaml",
            "",
            template("helm", "yaml"),
        ),
        ("deploy/chart/templates/_helpers.tpl", "", tree("helm")),
        (
            "deploy/chart/templates/NOTES.txt",
            "",
            template_plain("helm"),
        ),
        // `.tpl` is Helm only under `templates/`.
        ("deploy/other/_helpers.tpl", "", Syntax::Plain),
        // Go templates, by the path without `.gotmpl` or `.tmpl`
        (
            "deploy/helmfile.yaml.gotmpl",
            "",
            template("gotmpl", "yaml"),
        ),
        ("deploy/config.json.gotmpl", "", template("gotmpl", "json")),
        ("deploy/message.txt.gotmpl", "", template_plain("gotmpl")),
        ("deploy/page.gotmpl", "", tree("gotmpl")),
        ("scripts/setup.tmpl", bash, template("gotmpl", "bash")),
        // chezmoi's names
        (
            "dotfiles/dot_gitconfig.tmpl",
            "",
            template_bat("gotmpl", "Git Config"),
        ),
        (
            "dotfiles/dot_bash_profile.tmpl",
            "",
            template("gotmpl", "bash"),
        ),
        (
            "dotfiles/run_once_setup.sh.tmpl",
            "",
            template("gotmpl", "bash"),
        ),
        (
            "dotfiles/run_onchange_install-packages.tmpl",
            bash,
            template("gotmpl", "bash"),
        ),
        // Jinja, its suffix ahead of every file name and glob
        ("deploy/nginx.conf.j2", "", template_bat("jinja", "nginx")),
        ("deploy/settings.yaml.j2", "", template("jinja", "yaml")),
        (
            "web/templates/page.html.jinja",
            "",
            template("jinja", "html"),
        ),
        ("deploy/Dockerfile.j2", "", template("jinja", "dockerfile")),
        // A Jinja template under Helm's `templates/` is still Jinja.
        (
            "roles/web/templates/site.yaml.j2",
            "",
            template("jinja", "yaml"),
        ),
        // ERB, EJS and Twig
        ("web/views/layout.html.erb", "", template("erb", "html")),
        ("config/database.yml.erb", "", template("erb", "yaml")),
        ("web/views/item.ejs", "", tree("ejs")),
        ("web/views/list.html.ejs", "", template("ejs", "html")),
        ("web/templates/card.html.twig", "", template("twig", "html")),
        // An ordinary chezmoi source whose path says nothing takes its target's language.
        ("dotfiles/dot_zshrc", "", tree("bash")),
    ];
    for (path, text, syntax) in cases {
        assert_eq!(resolve(path, text).syntax, syntax, "{path}");
    }
}

/// chezmoi's source kinds decide before its target names: a symbolic link holds a path, a
/// modify script is what its `#!` line runs, or its target's language only as a modify
/// template, and a script chezmoi runs is what its `#!` line runs before what its name says.
#[test]
fn chezmoi_source_kinds_decide_first() {
    let python = indoc! {"
        #!/usr/bin/env python3
        import json
    "};
    let modify_template = indoc! {"
        {{- /* chezmoi:modify-template */ -}}
        {}
    "};
    let cases: [(&str, &str, Syntax); 7] = [
        (
            "dotfiles/symlink_dot_vimrc.tmpl",
            "",
            template_plain("gotmpl"),
        ),
        ("dotfiles/symlink_dot_vimrc", "", Syntax::Plain),
        (
            "dotfiles/modify_private_dot_claude.json.tmpl",
            "",
            template_plain("gotmpl"),
        ),
        (
            "dotfiles/modify_private_dot_claude.json.tmpl",
            python,
            template("gotmpl", "python"),
        ),
        (
            "dotfiles/modify_private_dot_claude.json.tmpl",
            modify_template,
            template("gotmpl", "json"),
        ),
        (
            "dotfiles/run_once_setup.sh.tmpl",
            python,
            template("gotmpl", "python"),
        ),
        // Without a `#!` line, an ordinary modify script is plain.
        ("dotfiles/modify_dot_config", "", Syntax::Plain),
    ];
    for (path, text, syntax) in cases {
        assert_eq!(resolve(path, text).syntax, syntax, "{path}: {text:?}");
    }
}

/// A Helm template's text is YAML: keys and values take YAML's tones, while its actions take
/// Go templates', and template syntax leaves no breadcrumbs.
#[test]
fn helm_templates_highlight_yaml_and_actions() {
    let text = indoc! {r#"
        apiVersion: apps/v1
        metadata:
          name: {{ include "chart.fullname" . }}
          labels:
            {{- include "chart.labels" . | nindent 4 }}
        spec:
          replicas: {{ .Values.replicaCount }}
    "#};
    expect_tones(
        "chart/templates/deployment.yaml",
        text,
        &template("helm", "yaml"),
        &[
            ("⟨apiVersion⟩:", Tone::Property),
            ("replicas⟨:⟩", Tone::Punctuation),
            ("{{ ⟨include⟩", Tone::Function),
            ("⟨\"chart.fullname\"⟩", Tone::String),
            ("⟨{{⟩ .Values", Tone::Punctuation),
            (".Values.⟨replicaCount⟩", Tone::Property),
        ],
    );
    assert_eq!(
        resolve("chart/templates/deployment.yaml", text).scopes,
        None
    );
}

/// A Go template producing JSON has JSON's tones in its text, not HTML's, so the variant's
/// data pattern wins over the row's own HTML injection.
#[test]
fn a_json_template_highlights_json() {
    let text = indoc! {r#"
        {
          "name": "{{ .name }}",
          "replicas": 3,
          "debug": false
        }
    "#};
    expect_tones(
        "deploy/config.json.gotmpl",
        text,
        &template("gotmpl", "json"),
        &[
            ("⟨\"replicas\"⟩", Tone::Property),
            ("⟨3⟩", Tone::Constant),
            ("⟨false⟩", Tone::Constant),
            ("{{ .⟨name⟩", Tone::Property),
        ],
    );
}

/// A template whose data is a bat syntax goes through the bridge to bat: nginx's directives in
/// a Jinja template take bat's tones, comments included, which contain anonymous tokens.
#[test]
fn a_jinja_template_highlights_nginx_through_bat() {
    let text = indoc! {"
        # Proxies the application.
        server {
            listen {{ nginx_port }};
            server_name {{ server_name }};
        }
    "};
    expect_tones(
        "deploy/nginx.conf.j2",
        text,
        &template_bat("jinja", "nginx"),
        &[
            ("⟨# Proxies the application.⟩", Tone::Comment),
            ("⟨server⟩ {", Tone::Keyword),
            ("⟨listen⟩", Tone::Keyword),
            ("⟨{{⟩ nginx_port", Tone::Keyword),
        ],
    );
}

/// A Jinja template producing YAML keeps YAML's comments, whose `#` is an anonymous token of
/// Jinja's text, and its `elif` branch's body keeps the data's tones, with only the `elif`
/// keyword taken as Jinja's.
#[test]
fn a_jinja_template_highlights_yaml_and_its_branches() {
    let text = indoc! {"
        # The service's settings.
        name: {{ name }}
        {% if debug %}
        level: debug
        {% elif verbose %}
        level: info
        {% endif %}
    "};
    expect_tones(
        "deploy/settings.yaml.j2",
        text,
        &template("jinja", "yaml"),
        &[
            ("⟨# The service's settings.⟩", Tone::Comment),
            ("⟨name⟩:", Tone::Property),
            ("{% ⟨if⟩", Tone::Keyword),
            ("{% ⟨elif⟩", Tone::Keyword),
            ("{% elif ⟨verbose⟩", Tone::Plain),
            ("⟨level⟩: info", Tone::Property),
            ("level: ⟨info⟩", Tone::String),
        ],
    );
}

/// A template whose name merely looks like a chezmoi source is no chezmoi source: an ERB view
/// named `modify_…` is HTML, while a chezmoi template of the same shape is a modify script.
#[test]
fn only_chezmoi_templates_read_chezmoi_names() {
    assert_eq!(
        resolve("web/views/modify_user.html.erb", "").syntax,
        template("erb", "html")
    );
    assert_eq!(
        resolve("dotfiles/modify_user.html.tmpl", "").syntax,
        template_plain("gotmpl")
    );
    assert_eq!(
        resolve("web/views/symlink_card.html.twig", "").syntax,
        template("twig", "html")
    );
}

/// A line shaped like a Jinja line statement, such as a YAML comment that starts with `# if`, is
/// literal text: Jinja runs line statements only when configured to, so the comment keeps YAML's
/// comment tone.
#[test]
fn a_jinja_line_statement_is_data() {
    let text = indoc! {"
        # if enabled, the ledger reconciles nightly.
        reconcile: {{ reconcile }}
    "};
    expect_tones(
        "deploy/settings.yaml.j2",
        text,
        &template("jinja", "yaml"),
        &[
            (
                "⟨# if enabled, the ledger reconciles nightly.⟩",
                Tone::Comment,
            ),
            ("⟨reconcile⟩:", Tone::Property),
        ],
    );
}

/// A line shaped like a line statement is literal text over plain data too, where nothing would
/// paint over a capture of Jinja's, so `# if enabled` stays plain.
#[test]
fn a_jinja_line_statement_over_plain_data_stays_plain() {
    let text = indoc! {"
        # if enabled
        {% if greeting %}Hello{% endif %}
    "};
    expect_tones(
        "deploy/message.txt.j2",
        text,
        &template_plain("jinja"),
        &[
            ("⟨# if enabled⟩", Tone::Plain),
            ("{% ⟨if⟩ greeting", Tone::Keyword),
            ("⟨Hello⟩", Tone::Plain),
        ],
    );
}

/// A line shaped like a Jinja line comment is literal text, as Jinja disables line comments
/// unless configured to: a Markdown heading keeps its tone, plain data stays plain, and a block
/// comment is Jinja's.
#[test]
fn a_jinja_line_comment_is_data() {
    let text = indoc! {"
        ## Heading
        {# A note for the template's authors. #}
        Text for {{ name }}.
    "};
    expect_tones(
        "docs/README.md.j2",
        text,
        &template("jinja", "markdown"),
        &[
            ("⟨## Heading⟩", Tone::Heading),
            ("⟨{# A note for the template's authors. #}⟩", Tone::Comment),
        ],
    );
    expect_tones(
        "docs/notes.txt.j2",
        text,
        &template_plain("jinja"),
        &[
            ("⟨## Heading⟩", Tone::Plain),
            ("⟨{# A note for the template's authors. #}⟩", Tone::Comment),
        ],
    );
}

/// An expression on a line shaped like a line comment is a real expression: in a Markdown
/// heading, `{{ title }}` takes the template's tones while `##` stays the heading's.
#[test]
fn an_expression_after_a_hash_heading_is_jinja() {
    let text = indoc! {"
        ## {{ title }}

        Text.
    "};
    expect_tones(
        "docs/README.md.j2",
        text,
        &template("jinja", "markdown"),
        &[
            ("⟨##⟩ {{", Tone::Heading),
            ("## ⟨{{⟩", Tone::Keyword),
            ("{{ ⟨title⟩", Tone::Plain),
        ],
    );
}

/// Lines shaped like a line-form raw block are text, so the expression between them is a real
/// expression, not raw text.
#[test]
fn a_hash_raw_line_opens_no_raw_block() {
    let text = indoc! {"
        # raw
        name: {{ name | upper }}
        # endraw
    "};
    expect_tones(
        "deploy/settings.yaml.j2",
        text,
        &template("jinja", "yaml"),
        &[
            ("⟨# raw⟩", Tone::Comment),
            ("name: ⟨{{⟩", Tone::Keyword),
            ("{{ name ⟨|⟩", Tone::Operator),
            ("⟨# endraw⟩", Tone::Comment),
        ],
    );
}

/// A raw block's body is data: HTML inside it takes HTML's tones, and with plain data it stays
/// plain, while the raw block's tags are Jinja's.
#[test]
fn a_jinja_raw_block_is_data() {
    let text = indoc! {"
        {% raw %}<b>{{ not jinja }}</b>{% endraw %}
    "};
    expect_tones(
        "web/templates/raw.html.jinja",
        text,
        &template("jinja", "html"),
        &[
            ("{% raw %}<⟨b⟩>", Tone::Tag),
            ("⟨{% raw %}⟩", Tone::Keyword),
        ],
    );
    expect_tones(
        "web/templates/raw.txt.jinja",
        text,
        &template_plain("jinja"),
        &[
            ("{% raw %}<⟨b⟩>", Tone::Plain),
            ("⟨{% raw %}⟩", Tone::Keyword),
        ],
    );
}

/// ERB's code is Ruby and its text the produced file's language.
#[test]
fn erb_code_is_ruby() {
    let text = indoc! {"
        <ul>
        <% @items.each do |item| %>
          <li><%= item.name %></li>
        <% end %>
        </ul>
    "};
    expect_tones(
        "web/views/layout.html.erb",
        text,
        &template("erb", "html"),
        &[
            ("<⟨ul⟩>", Tone::Tag),
            ("⟨do⟩ |item|", Tone::Keyword),
            ("⟨@items⟩", Tone::Property),
            ("<⟨li⟩>", Tone::Tag),
        ],
    );
}

/// A Jinja template producing a Dockerfile, named by its suffix ahead of Dockerfile's own
/// file name, highlights Dockerfile instructions.
///
/// The data is parsed joined, without the template's actions, so the sample keeps a valid
/// Dockerfile where they are left out.
#[test]
fn a_jinja_dockerfile_highlights_instructions() {
    let text = indoc! {r#"
        FROM python:3.12-slim
        ENV APP_VERSION="{{ version }}"
        RUN pip install "app=={{ version }}"
    "#};
    expect_tones(
        "Dockerfile.j2",
        text,
        &template("jinja", "dockerfile"),
        &[
            ("⟨FROM⟩", Tone::Keyword),
            ("⟨ENV⟩", Tone::Keyword),
            ("⟨RUN⟩", Tone::Keyword),
            ("APP_VERSION=\"{{ ⟨version⟩", Tone::Plain),
        ],
    );
}

/// Helm's `NOTES.txt` is plain text: only its template actions are highlighted.
#[test]
fn helm_notes_are_plain_text() {
    let text = indoc! {"
        Visit http://{{ .Values.host }} to use the application.
    "};
    expect_tones(
        "chart/templates/NOTES.txt",
        text,
        &template_plain("helm"),
        &[
            ("⟨Visit⟩", Tone::Plain),
            ("⟨to use⟩", Tone::Plain),
            ("⟨{{⟩ .Values", Tone::Punctuation),
        ],
    );
}

/// A Jinja fence in Markdown takes the plain Jinja row, whose own injections give its text
/// HTML; what neither language captures keeps the fence's code tone.
#[test]
fn an_injected_jinja_fence_highlights_html() {
    let text = indoc! {"
        # Template

        ```jinja
        <div>{{ greeting }}</div>
        ```
    "};
    expect_tones(
        "README.md",
        text,
        &tree("markdown"),
        &[
            ("<⟨div⟩>", Tone::Tag),
            ("⟨{{⟩ greeting", Tone::Keyword),
            ("{{ ⟨greeting⟩", Tone::Raw),
        ],
    );
}

/// A Go template producing Markdown with a Go template fence in it: the fence takes the plain
/// Go template row, whose text is HTML, not the outer template's Markdown.
#[test]
fn a_nested_template_fence_keeps_its_default() {
    let text = indoc! {"
        # {{ .Title }}

        ```gotmpl
        <p>{{ .Name }}</p>
        ```
    "};
    expect_tones(
        "docs/README.md.gotmpl",
        text,
        &template("gotmpl", "markdown"),
        &[
            ("⟨#⟩ {{", Tone::Heading),
            ("⟨{{⟩ .Title", Tone::Punctuation),
            ("<⟨p⟩>", Tone::Tag),
        ],
    );
}

/// The same blob highlights differently as two names whose data differs, since the resolved
/// syntax is part of what identifies its highlights.
#[test]
fn one_blob_highlights_as_each_data_language() {
    let text = "debug: {{ debug }}\n";
    let yaml = analyze("deploy/settings.yaml.j2", text);
    let plain = analyze("deploy/settings.txt.j2", text);
    let tone = |syntax: &SideSyntax| tone_at(syntax, 0, 0);
    assert_eq!(tone(&yaml), Tone::Property);
    assert_eq!(tone(&plain), Tone::Plain);
    assert_ne!(yaml, plain);
}

/// Every template row's data nodes exist in its grammar: each variant's appended pattern
/// compiles, for data in either tier and plain data.
#[test]
fn every_template_rows_data_nodes_exist() {
    let languages = Languages::standard();
    let mut templates = 0;
    for (index, row) in registry::rows().iter().enumerate() {
        if !row.is_template() {
            continue;
        }
        templates += 1;
        for data in [
            Data::TreeHouse("yaml"),
            Data::Bat("nginx".to_owned()),
            Data::Plain,
        ] {
            if let Err(err) = languages::build_variant(row, &data) {
                panic!(
                    "{}'s variant with {data:?}: {err}: {}",
                    row.name,
                    std::error::Error::source(&err).map_or_else(String::new, ToString::to_string)
                );
            }
            assert!(
                languages.variant(index, &data).is_some(),
                "{} {data:?}",
                row.name
            );
        }
    }
    assert_eq!(templates, 6);
}
