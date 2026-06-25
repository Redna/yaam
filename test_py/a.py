from test_py.b import BaseClass, helper_func

class DerivedClass(BaseClass):
    def derived_method(self):
        self.base_method()
        val = helper_func()
        print("derived method", val)
