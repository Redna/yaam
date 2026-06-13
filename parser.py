import os
import tree_sitter_python as tspython
from tree_sitter import Language, Parser

def extract_entities(file_path):
    """Extracts classes, top-level functions, and class methods from a Python file."""
    if not file_path.endswith(".py") or not os.path.exists(file_path):
        return {"classes": {}, "top_level_functions": []}
    
    PY_LANGUAGE = Language(tspython.language())
    parser = Parser(PY_LANGUAGE)
    
    with open(file_path, "rb") as f:
        tree = parser.parse(f.read())
        
    entities = {
        "classes": {}, # class_name -> list of method names
        "top_level_functions": [] # list of function names
    }
    
    def traverse(node, current_class=None):
        if node.type == 'class_definition':
            name_node = node.child_by_field_name('name')
            if name_node:
                class_name = name_node.text.decode('utf-8')
                entities["classes"][class_name] = []
                # Walk children of the class with class context
                for child in node.children:
                    traverse(child, current_class=class_name)
                return # Avoid double traversal of children
        elif node.type == 'function_definition':
            name_node = node.child_by_field_name('name')
            if name_node:
                func_name = name_node.text.decode('utf-8')
                if current_class:
                    entities["classes"][current_class].append(func_name)
                else:
                    entities["top_level_functions"].append(func_name)
            return # Ignore local functions inside function definitions
            
        for child in node.children:
            traverse(child, current_class)
            
    traverse(tree.root_node)
    return entities

def extract_functions(file_path):
    """Extracts all function and method names as a flat list for backward compatibility."""
    entities = extract_entities(file_path)
    funcs = list(entities["top_level_functions"])
    for methods in entities["classes"].values():
        funcs.extend(methods)
    return funcs
