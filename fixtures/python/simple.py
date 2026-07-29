import os

class Greeter:
    """A friendly class."""
    def greet(self, name: str) -> str:
        return f"Hello, {name}"

def main():
    g = Greeter()
    g.greet("world")
