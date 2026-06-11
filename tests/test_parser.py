import unittest
import os
from parser import extract_functions

class TestParser(unittest.TestCase):
    def test_extract_functions(self):
        test_content = """
def hello():
    print("world")

class MyClass:
    def method(self):
        pass

def another_func(a, b):
    return a + b
"""
        test_file = "temp_test_parser.py"
        with open(test_file, "w") as f:
            f.write(test_content)
        
        try:
            funcs = extract_functions(test_file)
            # method() is inside a class, so it might not be captured by the simple query 
            # unless we specifically target class methods.
            # Current query: (function_definition name: (identifier) @function.name)
            # This captures top-level functions and method definitions.
            self.assertIn("hello", funcs)
            self.assertIn("another_func", funcs)
            self.assertIn("method", funcs)
        finally:
            if os.path.exists(test_file):
                os.remove(test_file)

if __name__ == "__main__":
    unittest.main()
