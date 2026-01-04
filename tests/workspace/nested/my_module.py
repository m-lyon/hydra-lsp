class ClassA:
    def __init__(self, a_value: int, b: "ClassB"):
        self.a_value = a_value
        self.b = b


class ClassB:
    def __init__(self, b_value: int, c: "ClassC"):
        self.b_value = b_value
        self.c = c


class ClassC:
    def __init__(self, d: "ClassD"):
        self.d = d


class ClassD:
    def __init__(self, d_value: int):
        self.value = d_value
