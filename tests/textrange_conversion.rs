use hydra_lsp::python_analyzer::{DefinitionInfo, PythonAnalyzer};
use std::fs;
use std::io::Write;
use tempfile::{NamedTempFile, TempDir};

#[test]
fn test_function_line_number_conversion() {
    // Create a test Python file with a function at a known position
    let source = r#"# This is line 0
# This is line 1

def my_function(x: int, y: int) -> int:
    """A test function."""
    return x + y

class MyClass:
    """A test class."""
    
    def __init__(self, value: int):
        """Initialize the class."""
        self.value = value
"#;
    let mut test_file = NamedTempFile::new().unwrap();
    test_file.write_all(source.as_bytes()).unwrap();
    test_file.flush().unwrap();

    // Test function extraction
    let func_sig =
        PythonAnalyzer::extract_function_signature(test_file.path(), "my_function").unwrap();

    // The function should start at line 3
    assert_eq!(func_sig.start_line, 3, "Function should start at line 3");
    assert_eq!(
        func_sig.start_column, 0,
        "Function should start at column 0"
    );

    // The function name is "my_function"
    assert_eq!(func_sig.name, "my_function");

    // The function should have proper end position
    assert!(
        func_sig.end_line >= func_sig.start_line,
        "End line should be >= start line"
    );
}

#[test]
fn test_class_line_number_conversion() {
    let source = r#"# This is line 0
# This is line 1
# This is line 2
# This is line 3
# This is line 4

class MyClass:
    """A test class on line 6."""
    
    def __init__(self, value: int):
        """Initialize the class."""
        self.value = value
    
    def method(self):
        pass
"#;
    let mut test_file = NamedTempFile::new().unwrap();
    test_file.write_all(source.as_bytes()).unwrap();
    test_file.flush().unwrap();

    // Test class extraction
    let class_info = PythonAnalyzer::extract_class_info(test_file.path(), "MyClass").unwrap();

    assert_eq!(class_info.start_line, 6, "Class should start at line 6");
    assert_eq!(class_info.start_column, 0, "Class should start at column 0");

    // The class name is "MyClass"
    assert_eq!(class_info.name, "MyClass");

    // The __init__ method should be extracted
    assert!(
        class_info.init_signature.is_some(),
        "__init__ should be found"
    );

    let init_sig = class_info.init_signature.unwrap();
    // __init__ should start at line 9
    assert_eq!(init_sig.start_line, 9, "__init__ should start at line 9");
}

#[test]
fn test_indented_function() {
    let source = r#"class Container:
    def nested_function(self, arg: str) -> str:
        """A nested function."""
        return arg.upper()
"#;
    let mut test_file = NamedTempFile::new().unwrap();
    test_file.write_all(source.as_bytes()).unwrap();
    test_file.flush().unwrap();

    // Test that we can extract the nested function
    let func_sig =
        PythonAnalyzer::extract_function_signature(test_file.path(), "nested_function").unwrap();
    // The function should start at line 1
    assert_eq!(
        func_sig.start_line, 1,
        "Nested function should start at line 1"
    );
    // The function should start at column 4
    assert_eq!(
        func_sig.start_column, 4,
        "Nested function should start at column 4"
    );

    assert_eq!(func_sig.name, "nested_function");
}

#[test]
fn test_multiline_function() {
    let source = r#"def multiline_function(
    param1: int,
    param2: str,
    param3: bool
) -> tuple[int, str, bool]:
    """A function with multiple lines."""
    return (param1, param2, param3)
"#;
    let mut test_file = NamedTempFile::new().unwrap();
    test_file.write_all(source.as_bytes()).unwrap();
    test_file.flush().unwrap();

    let func_sig =
        PythonAnalyzer::extract_function_signature(test_file.path(), "multiline_function").unwrap();

    assert_eq!(
        func_sig.start_line, 0,
        "Multiline function should start at line 0"
    );
    assert_eq!(
        func_sig.start_column, 0,
        "Multiline function should start at column 0"
    );

    // The end line should be after the start line (function spans multiple lines)
    assert_eq!(func_sig.end_line, 6, "Function should end at line 6");
}

#[test]
fn test_byte_offset_not_line_number() {
    // This test verifies that we're correctly converting byte offsets to line numbers
    // A file where byte offset and line number would be very different
    let source = r#"# Line 0: This is a comment with some content to increase byte offset
# Line 1: More comments to push the byte offset higher
# Line 2: Even more comments
# Line 3: Keep adding comments
# Line 4: One more comment line

def target_function() -> None:
    """This function is at line 6."""
    pass
"#;
    let mut test_file = NamedTempFile::new().unwrap();
    test_file.write_all(source.as_bytes()).unwrap();
    test_file.flush().unwrap();

    let func_sig =
        PythonAnalyzer::extract_function_signature(test_file.path(), "target_function").unwrap();

    assert_eq!(func_sig.start_line, 6, "Function should be at line 6");
}

#[test]
fn test_utf8_multibyte_characters() {
    // Test with UTF-8 multibyte characters to ensure proper handling
    let source = r#"# 中文注释
# Another comment: 日本語

def unicode_function(name: str) -> str:
    """Function with unicode: émojis 🚀 こんにちは"""
    return f"Hello, {name}!"
"#;
    let mut test_file = NamedTempFile::new().unwrap();
    test_file.write_all(source.as_bytes()).unwrap();
    test_file.flush().unwrap();

    let func_sig =
        PythonAnalyzer::extract_function_signature(test_file.path(), "unicode_function").unwrap();

    assert_eq!(
        func_sig.start_line, 3,
        "Function with unicode should start at line 3"
    );
    assert_eq!(
        func_sig.start_column, 0,
        "Function should start at column 0"
    );
}

#[test]
fn test_extract_definition_info_function() {
    // Test the higher-level extract_definition_info method
    let source = r#"# Line 0
# Line 1

def test_function(x: int) -> int:
    """Test function at line 3."""
    return x * 2
"#;
    let temp_dir = TempDir::new().unwrap();
    let workspace_root = temp_dir.path();
    let workspace_dir = workspace_root.join("workspace");
    fs::create_dir_all(&workspace_dir).unwrap();
    let test_file = workspace_dir.join("test_module.py");
    fs::write(&test_file, source).unwrap();

    let (def_info, _file_path, _module_path, _symbol_name) =
        PythonAnalyzer::extract_definition_info(
            "workspace.test_module.test_function",
            Some(workspace_root),
            None,
        )
        .unwrap();

    match def_info {
        DefinitionInfo::Function(sig) => {
            assert_eq!(sig.start_line, 3, "Function should be at line 3");
            assert_eq!(sig.name, "test_function");
        }
        _ => panic!("Expected a function definition"),
    }

    // Clean up
    fs::remove_file(test_file).ok();
}

#[test]
fn test_extract_definition_info_class() {
    let source = r#"# Line 0
# Line 1
# Line 2

class TestClass:
    """Test class at line 4."""
    
    def __init__(self):
        """Initialize at line 7."""
        pass
"#;
    let temp_dir = TempDir::new().unwrap();
    let workspace_root = temp_dir.path();
    let workspace_dir = workspace_root.join("workspace");
    fs::create_dir_all(&workspace_dir).unwrap();
    let test_file = workspace_dir.join("test_class_module.py");
    fs::write(&test_file, source).unwrap();

    let (def_info, _file_path, _module_path, _symbol_name) =
        PythonAnalyzer::extract_definition_info(
            "workspace.test_class_module.TestClass",
            Some(workspace_root),
            None,
        )
        .unwrap();

    match def_info {
        DefinitionInfo::Class(class_info) => {
            assert_eq!(class_info.start_line, 4, "Class should be at line 4");
            assert_eq!(class_info.name, "TestClass");

            // Check __init__ line number
            let init_sig = class_info.init_signature.unwrap();
            assert_eq!(init_sig.start_line, 7, "__init__ should be at line 7");
        }
        _ => panic!("Expected a class definition"),
    }

    // Clean up
    fs::remove_file(test_file).ok();
}
