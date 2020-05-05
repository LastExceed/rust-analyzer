//! FIXME: write short doc here

use hir::{ModuleSource, Semantics};
use ra_db::{RelativePath, RelativePathBuf, SourceDatabaseExt};
use ra_ide_db::RootDatabase;
use ra_syntax::{
    algo::find_node_at_offset, ast, lex_single_valid_syntax_kind, AstNode, SyntaxKind, SyntaxNode,
};
use ra_text_edit::TextEdit;
use test_utils::tested_by;

use crate::{
    references::find_all_refs, FilePosition, FileSystemEdit, RangeInfo, Reference, ReferenceKind,
    SourceChange, SourceFileEdit, TextRange,
};

pub(crate) fn rename(
    db: &RootDatabase,
    position: FilePosition,
    new_name: &str,
) -> Option<RangeInfo<SourceChange>> {
    match lex_single_valid_syntax_kind(new_name)? {
        SyntaxKind::IDENT | SyntaxKind::UNDERSCORE => (),
        _ => return None,
    }

    let sema = Semantics::new(db);
    let source_file = sema.parse(position.file_id);
    if let Some((ast_name, ast_module)) =
        find_name_and_module_at_offset(source_file.syntax(), position)
    {
        let range = ast_name.syntax().text_range();
        rename_mod(&sema, &ast_name, &ast_module, position, new_name)
            .map(|info| RangeInfo::new(range, info))
    } else {
        rename_reference(sema.db, position, new_name)
    }
}

fn find_name_and_module_at_offset(
    syntax: &SyntaxNode,
    position: FilePosition,
) -> Option<(ast::Name, ast::Module)> {
    let ast_name = find_node_at_offset::<ast::Name>(syntax, position.offset)?;
    let ast_module = ast::Module::cast(ast_name.syntax().parent()?)?;
    Some((ast_name, ast_module))
}

fn source_edit_from_reference(reference: Reference, new_name: &str) -> SourceFileEdit {
    let mut replacement_text = String::new();
    let file_id = reference.file_range.file_id;
    let range = match reference.kind {
        ReferenceKind::FieldShorthandForField => {
            tested_by!(test_rename_struct_field_for_shorthand);
            replacement_text.push_str(new_name);
            replacement_text.push_str(": ");
            TextRange::new(reference.file_range.range.start(), reference.file_range.range.start())
        }
        ReferenceKind::FieldShorthandForLocal => {
            tested_by!(test_rename_local_for_field_shorthand);
            replacement_text.push_str(": ");
            replacement_text.push_str(new_name);
            TextRange::new(reference.file_range.range.end(), reference.file_range.range.end())
        }
        _ => {
            replacement_text.push_str(new_name);
            reference.file_range.range
        }
    };
    SourceFileEdit { file_id, edit: TextEdit::replace(range, replacement_text) }
}

fn rename_mod(
    sema: &Semantics<RootDatabase>,
    ast_name: &ast::Name,
    ast_module: &ast::Module,
    position: FilePosition,
    new_name: &str,
) -> Option<SourceChange> {
    let mut source_file_edits = Vec::new();
    let mut file_system_edits = Vec::new();
    if let Some(module) = sema.to_def(ast_module) {
        let src = module.definition_source(sema.db);
        let file_id = src.file_id.original_file(sema.db);
        match src.value {
            ModuleSource::SourceFile(..) => {
                let mod_path: RelativePathBuf = sema.db.file_relative_path(file_id);
                // mod is defined in path/to/dir/mod.rs
                let dst_path = if mod_path.file_stem() == Some("mod") {
                    mod_path
                        .parent()
                        .and_then(|p| p.parent())
                        .or_else(|| Some(RelativePath::new("")))
                        .map(|p| p.join(new_name).join("mod.rs"))
                } else {
                    Some(mod_path.with_file_name(new_name).with_extension("rs"))
                };
                if let Some(path) = dst_path {
                    let move_file = FileSystemEdit::MoveFile {
                        src: file_id,
                        dst_source_root: sema.db.file_source_root(position.file_id),
                        dst_path: path,
                    };
                    file_system_edits.push(move_file);
                }
            }
            ModuleSource::Module(..) => {}
        }
    }

    let edit = SourceFileEdit {
        file_id: position.file_id,
        edit: TextEdit::replace(ast_name.syntax().text_range(), new_name.into()),
    };
    source_file_edits.push(edit);

    if let Some(RangeInfo { range: _, info: refs }) = find_all_refs(sema.db, position, None) {
        let ref_edits = refs
            .references
            .into_iter()
            .map(|reference| source_edit_from_reference(reference, new_name));
        source_file_edits.extend(ref_edits);
    }

    Some(SourceChange::from_edits("Rename", source_file_edits, file_system_edits))
}

fn rename_reference(
    db: &RootDatabase,
    position: FilePosition,
    new_name: &str,
) -> Option<RangeInfo<SourceChange>> {
    let RangeInfo { range, info: refs } = find_all_refs(db, position, None)?;

    let edit = refs
        .into_iter()
        .map(|reference| source_edit_from_reference(reference, new_name))
        .collect::<Vec<_>>();

    if edit.is_empty() {
        return None;
    }

    Some(RangeInfo::new(range, SourceChange::source_file_edits("Rename", edit)))
}

#[cfg(test)]
mod tests {
    use insta::assert_debug_snapshot;
    use ra_text_edit::TextEditBuilder;
    use test_utils::{assert_eq_text, covers};

    use crate::{
        mock_analysis::analysis_and_position, mock_analysis::single_file_with_position, FileId,
    };

    #[test]
    fn test_rename_to_underscore() {
        test_rename(
            r#"
    fn main() {
        let i<|> = 1;
    }"#,
            "_",
            r#"
    fn main() {
        let _ = 1;
    }"#,
        );
    }

    #[test]
    fn test_rename_to_raw_identifier() {
        test_rename(
            r#"
    fn main() {
        let i<|> = 1;
    }"#,
            "r#fn",
            r#"
    fn main() {
        let r#fn = 1;
    }"#,
        );
    }

    #[test]
    fn test_rename_to_invalid_identifier() {
        let (analysis, position) = single_file_with_position(
            "
    fn main() {
        let i<|> = 1;
    }",
        );
        let new_name = "invalid!";
        let source_change = analysis.rename(position, new_name).unwrap();
        assert!(source_change.is_none());
    }

    #[test]
    fn test_rename_for_local() {
        test_rename(
            r#"
    fn main() {
        let mut i = 1;
        let j = 1;
        i = i<|> + j;

        {
            i = 0;
        }

        i = 5;
    }"#,
            "k",
            r#"
    fn main() {
        let mut k = 1;
        let j = 1;
        k = k + j;

        {
            k = 0;
        }

        k = 5;
    }"#,
        );
    }

    #[test]
    fn test_rename_for_macro_args() {
        test_rename(
            r#"
    macro_rules! foo {($i:ident) => {$i} }
    fn main() {
        let a<|> = "test";
        foo!(a);
    }"#,
            "b",
            r#"
    macro_rules! foo {($i:ident) => {$i} }
    fn main() {
        let b = "test";
        foo!(b);
    }"#,
        );
    }

    #[test]
    fn test_rename_for_macro_args_rev() {
        test_rename(
            r#"
    macro_rules! foo {($i:ident) => {$i} }
    fn main() {
        let a = "test";
        foo!(a<|>);
    }"#,
            "b",
            r#"
    macro_rules! foo {($i:ident) => {$i} }
    fn main() {
        let b = "test";
        foo!(b);
    }"#,
        );
    }

    #[test]
    fn test_rename_for_macro_define_fn() {
        test_rename(
            r#"
    macro_rules! define_fn {($id:ident) => { fn $id{} }}
    define_fn!(foo);
    fn main() {
        fo<|>o();
    }"#,
            "bar",
            r#"
    macro_rules! define_fn {($id:ident) => { fn $id{} }}
    define_fn!(bar);
    fn main() {
        bar();
    }"#,
        );
    }

    #[test]
    fn test_rename_for_macro_define_fn_rev() {
        test_rename(
            r#"
    macro_rules! define_fn {($id:ident) => { fn $id{} }}
    define_fn!(fo<|>o);
    fn main() {
        foo();
    }"#,
            "bar",
            r#"
    macro_rules! define_fn {($id:ident) => { fn $id{} }}
    define_fn!(bar);
    fn main() {
        bar();
    }"#,
        );
    }

    #[test]
    fn test_rename_for_param_inside() {
        test_rename(
            r#"
    fn foo(i : u32) -> u32 {
        i<|>
    }"#,
            "j",
            r#"
    fn foo(j : u32) -> u32 {
        j
    }"#,
        );
    }

    #[test]
    fn test_rename_refs_for_fn_param() {
        test_rename(
            r#"
    fn foo(i<|> : u32) -> u32 {
        i
    }"#,
            "new_name",
            r#"
    fn foo(new_name : u32) -> u32 {
        new_name
    }"#,
        );
    }

    #[test]
    fn test_rename_for_mut_param() {
        test_rename(
            r#"
    fn foo(mut i<|> : u32) -> u32 {
        i
    }"#,
            "new_name",
            r#"
    fn foo(mut new_name : u32) -> u32 {
        new_name
    }"#,
        );
    }

    #[test]
    fn test_rename_struct_field() {
        test_rename(
            r#"
    struct Foo {
        i<|>: i32,
    }

    impl Foo {
        fn new(i: i32) -> Self {
            Self { i: i }
        }
    }
    "#,
            "j",
            r#"
    struct Foo {
        j: i32,
    }

    impl Foo {
        fn new(i: i32) -> Self {
            Self { j: i }
        }
    }
    "#,
        );
    }

    #[test]
    fn test_rename_struct_field_for_shorthand() {
        covers!(test_rename_struct_field_for_shorthand);
        test_rename(
            r#"
    struct Foo {
        i<|>: i32,
    }

    impl Foo {
        fn new(i: i32) -> Self {
            Self { i }
        }
    }
    "#,
            "j",
            r#"
    struct Foo {
        j: i32,
    }

    impl Foo {
        fn new(i: i32) -> Self {
            Self { j: i }
        }
    }
    "#,
        );
    }

    #[test]
    fn test_rename_local_for_field_shorthand() {
        covers!(test_rename_local_for_field_shorthand);
        test_rename(
            r#"
    struct Foo {
        i: i32,
    }

    impl Foo {
        fn new(i<|>: i32) -> Self {
            Self { i }
        }
    }
    "#,
            "j",
            r#"
    struct Foo {
        i: i32,
    }

    impl Foo {
        fn new(j: i32) -> Self {
            Self { i: j }
        }
    }
    "#,
        );
    }

    #[test]
    fn test_field_shorthand_correct_struct() {
        test_rename(
            r#"
    struct Foo {
        i<|>: i32,
    }

    struct Bar {
        i: i32,
    }

    impl Bar {
        fn new(i: i32) -> Self {
            Self { i }
        }
    }
    "#,
            "j",
            r#"
    struct Foo {
        j: i32,
    }

    struct Bar {
        i: i32,
    }

    impl Bar {
        fn new(i: i32) -> Self {
            Self { i }
        }
    }
    "#,
        );
    }

    #[test]
    fn test_shadow_local_for_struct_shorthand() {
        test_rename(
            r#"
    struct Foo {
        i: i32,
    }

    fn baz(i<|>: i32) -> Self {
         let x = Foo { i };
         {
             let i = 0;
             Foo { i }
         }
     }
    "#,
            "j",
            r#"
    struct Foo {
        i: i32,
    }

    fn baz(j: i32) -> Self {
         let x = Foo { i: j };
         {
             let i = 0;
             Foo { i }
         }
     }
    "#,
        );
    }

    #[test]
    fn test_rename_mod() {
        let (analysis, position) = analysis_and_position(
            "
            //- /lib.rs
            mod bar;

            //- /bar.rs
            mod foo<|>;

            //- /bar/foo.rs
            // emtpy
            ",
        );
        let new_name = "foo2";
        let source_change = analysis.rename(position, new_name).unwrap();
        assert_debug_snapshot!(&source_change,
@r###"
        Some(
            RangeInfo {
                range: 4..7,
                info: SourceChange {
                    label: "Rename",
                    source_file_edits: [
                        SourceFileEdit {
                            file_id: FileId(
                                2,
                            ),
                            edit: TextEdit {
                                atoms: [
                                    AtomTextEdit {
                                        delete: 4..7,
                                        insert: "foo2",
                                    },
                                ],
                            },
                        },
                    ],
                    file_system_edits: [
                        MoveFile {
                            src: FileId(
                                3,
                            ),
                            dst_source_root: SourceRootId(
                                0,
                            ),
                            dst_path: "bar/foo2.rs",
                        },
                    ],
                    cursor_position: None,
                },
            },
        )
        "###);
    }

    #[test]
    fn test_rename_mod_in_dir() {
        let (analysis, position) = analysis_and_position(
            "
            //- /lib.rs
            mod fo<|>o;
            //- /foo/mod.rs
            // emtpy
            ",
        );
        let new_name = "foo2";
        let source_change = analysis.rename(position, new_name).unwrap();
        assert_debug_snapshot!(&source_change,
        @r###"
        Some(
            RangeInfo {
                range: 4..7,
                info: SourceChange {
                    label: "Rename",
                    source_file_edits: [
                        SourceFileEdit {
                            file_id: FileId(
                                1,
                            ),
                            edit: TextEdit {
                                atoms: [
                                    AtomTextEdit {
                                        delete: 4..7,
                                        insert: "foo2",
                                    },
                                ],
                            },
                        },
                    ],
                    file_system_edits: [
                        MoveFile {
                            src: FileId(
                                2,
                            ),
                            dst_source_root: SourceRootId(
                                0,
                            ),
                            dst_path: "foo2/mod.rs",
                        },
                    ],
                    cursor_position: None,
                },
            },
        )
        "###
               );
    }

    #[test]
    fn test_module_rename_in_path() {
        test_rename(
            r#"
    mod <|>foo {
        pub fn bar() {}
    }

    fn main() {
        foo::bar();
    }"#,
            "baz",
            r#"
    mod baz {
        pub fn bar() {}
    }

    fn main() {
        baz::bar();
    }"#,
        );
    }

    #[test]
    fn test_rename_mod_filename_and_path() {
        let (analysis, position) = analysis_and_position(
            "
            //- /lib.rs
            mod bar;
            fn f() {
                bar::foo::fun()
            }

            //- /bar.rs
            pub mod foo<|>;

            //- /bar/foo.rs
            // pub fn fun() {}
            ",
        );
        let new_name = "foo2";
        let source_change = analysis.rename(position, new_name).unwrap();
        assert_debug_snapshot!(&source_change,
@r###"
        Some(
            RangeInfo {
                range: 8..11,
                info: SourceChange {
                    label: "Rename",
                    source_file_edits: [
                        SourceFileEdit {
                            file_id: FileId(
                                2,
                            ),
                            edit: TextEdit {
                                atoms: [
                                    AtomTextEdit {
                                        delete: 8..11,
                                        insert: "foo2",
                                    },
                                ],
                            },
                        },
                        SourceFileEdit {
                            file_id: FileId(
                                1,
                            ),
                            edit: TextEdit {
                                atoms: [
                                    AtomTextEdit {
                                        delete: 27..30,
                                        insert: "foo2",
                                    },
                                ],
                            },
                        },
                    ],
                    file_system_edits: [
                        MoveFile {
                            src: FileId(
                                3,
                            ),
                            dst_source_root: SourceRootId(
                                0,
                            ),
                            dst_path: "bar/foo2.rs",
                        },
                    ],
                    cursor_position: None,
                },
            },
        )
        "###);
    }

    fn test_rename(text: &str, new_name: &str, expected: &str) {
        let (analysis, position) = single_file_with_position(text);
        let source_change = analysis.rename(position, new_name).unwrap();
        let mut text_edit_builder = TextEditBuilder::default();
        let mut file_id: Option<FileId> = None;
        if let Some(change) = source_change {
            for edit in change.info.source_file_edits {
                file_id = Some(edit.file_id);
                for atom in edit.edit.as_atoms() {
                    text_edit_builder.replace(atom.delete, atom.insert.clone());
                }
            }
        }
        let result =
            text_edit_builder.finish().apply(&*analysis.file_text(file_id.unwrap()).unwrap());
        assert_eq_text!(expected, &*result);
    }
}
