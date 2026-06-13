import os
import tree_sitter_python as tspython
from tree_sitter import Language, Parser

def extract_entities(file_path):
    """Extracts classes, top-level functions, and class methods with their declaration line numbers."""
    if not file_path.endswith(".py") or not os.path.exists(file_path):
        return {"classes": {}, "top_level_functions": {}}
    
    PY_LANGUAGE = Language(tspython.language())
    parser = Parser(PY_LANGUAGE)
    
    with open(file_path, "rb") as f:
        tree = parser.parse(f.read())
        
    entities = {
        "classes": {}, # class_name -> {"line": int, "methods": {method_name: line_int}}
        "top_level_functions": {} # function_name -> line_int
    }
    
    def traverse(node, current_class=None):
        if node.type == 'class_definition':
            name_node = node.child_by_field_name('name')
            if name_node:
                class_name = name_node.text.decode('utf-8')
                class_line = node.start_point[0] + 1
                entities["classes"][class_name] = {"line": class_line, "methods": {}}
                # Walk children of the class with class context
                for child in node.children:
                    traverse(child, current_class=class_name)
                return # Avoid double traversal of children
        elif node.type == 'function_definition':
            name_node = node.child_by_field_name('name')
            if name_node:
                func_name = name_node.text.decode('utf-8')
                func_line = node.start_point[0] + 1
                if current_class:
                    entities["classes"][current_class]["methods"][func_name] = func_line
                else:
                    entities["top_level_functions"][func_name] = func_line
            return # Ignore local functions inside function definitions
            
        for child in node.children:
            traverse(child, current_class)
            
    traverse(tree.root_node)
    return entities

def extract_functions(file_path):
    """Extracts all function and method names as a flat list for backward compatibility."""
    entities = extract_entities(file_path)
    funcs = list(entities["top_level_functions"].keys())
    for class_info in entities["classes"].values():
        funcs.extend(class_info["methods"].keys())
    return funcs
