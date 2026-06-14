import os
import tree_sitter_python as tspython
from tree_sitter import Language, Parser

def extract_entities(file_path):
    """Extracts classes, top-level functions, and class methods with line numbers, superclasses, and call targets."""
    if not file_path.endswith(".py") or not os.path.exists(file_path):
        return {"classes": {}, "top_level_functions": {}, "imports": [], "top_level_calls": []}
    
    PY_LANGUAGE = Language(tspython.language())
    parser = Parser(PY_LANGUAGE)
    
    with open(file_path, "rb") as f:
        tree = parser.parse(f.read())
        
    entities = {
        "classes": {}, # class_name -> {"line": int, "superclasses": [...], "methods": {method_name: {"line": int, "calls": [...]}}}
        "top_level_functions": {}, # function_name -> {"line": int, "calls": [...]}
        "imports": [], # list of imported module names
        "top_level_calls": [] # list of call targets outside classes/functions
    }
    
    def traverse(node, current_class=None, current_function=None):
        if node.type == 'import_statement':
            for child in node.children:
                if child.type == 'dotted_name':
                    entities["imports"].append(child.text.decode('utf-8'))
                elif child.type == 'aliased_import':
                    for sub in child.children:
                        if sub.type == 'dotted_name':
                            entities["imports"].append(sub.text.decode('utf-8'))
        elif node.type == 'import_from_statement':
            for child in node.children:
                if child.type == 'dotted_name':
                    entities["imports"].append(child.text.decode('utf-8'))
                    break
        elif node.type == 'class_definition':
            name_node = node.child_by_field_name('name')
            if name_node:
                class_name = name_node.text.decode('utf-8')
                superclasses = []
                superclasses_node = node.child_by_field_name('superclasses')
                if superclasses_node:
                    for child in superclasses_node.children:
                        if child.type in ('identifier', 'attribute'):
                            superclasses.append(child.text.decode('utf-8'))
                entities["classes"][class_name] = {
                    "line": node.start_point[0] + 1,
                    "superclasses": superclasses,
                    "methods": {}
                }
                # Walk children of the class with class context
                for child in node.children:
                    traverse(child, current_class=class_name)
                return # Avoid double traversal of children
        elif node.type == 'function_definition':
            name_node = node.child_by_field_name('name')
            if name_node:
                func_name = name_node.text.decode('utf-8')
                func_line = node.start_point[0] + 1
                if current_function is not None:
                    # Skip nested function definitions (closures)
                    return
                if current_class:
                    entities["classes"][current_class]["methods"][func_name] = {
                        "line": func_line,
                        "calls": []
                    }
                    for child in node.children:
                        traverse(child, current_class=current_class, current_function=func_name)
                else:
                    entities["top_level_functions"][func_name] = {
                        "line": func_line,
                        "calls": []
                    }
                    for child in node.children:
                        traverse(child, current_function=func_name)
                return # Avoid double traversal
        elif node.type == 'call':
            func_node = node.child_by_field_name('function')
            if func_node:
                call_target = None
                if func_node.type == 'identifier':
                    call_target = func_node.text.decode('utf-8')
                elif func_node.type == 'attribute':
                    obj_node = func_node.child_by_field_name('object')
                    attr_node = func_node.child_by_field_name('attribute')
                    if obj_node and attr_node:
                        obj_name = obj_node.text.decode('utf-8')
                        attr_name = attr_node.text.decode('utf-8')
                        call_target = f"{obj_name}.{attr_name}"
                
                if call_target:
                    if current_class and current_function:
                        entities["classes"][current_class]["methods"][current_function]["calls"].append(call_target)
                    elif current_function:
                        entities["top_level_functions"][current_function]["calls"].append(call_target)
                    else:
                        entities["top_level_calls"].append(call_target)
            
        for child in node.children:
            traverse(child, current_class, current_function)
            
    traverse(tree.root_node)
    return entities

def extract_functions(file_path):
    """Extracts all function and method names as a flat list for backward compatibility."""
    entities = extract_entities(file_path)
    funcs = list(entities["top_level_functions"].keys())
    for class_info in entities["classes"].values():
        funcs.extend(class_info["methods"].keys())
    return funcs
