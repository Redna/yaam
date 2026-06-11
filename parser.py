import os
import tree_sitter_python as tspython
from tree_sitter import Language, Parser

def extract_functions(file_path):
    """Extracts function names from a Python file using Tree-sitter."""
    if not file_path.endswith(".py") or not os.path.exists(file_path):
        return []
    
    PY_LANGUAGE = Language(tspython.language())
    parser = Parser(PY_LANGUAGE)
    
    with open(file_path, "rb") as f:
        tree = parser.parse(f.read())
        
    query = PY_LANGUAGE.query("""
    (function_definition
      name: (identifier) @function.name)
    """)
    
    captures = query.captures(tree.root_node)
    functions = []
    for node, tag in captures:
        if tag == "function.name":
            functions.append(node.text.decode("utf-8"))
    return functions
