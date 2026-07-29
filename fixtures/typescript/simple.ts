import { add } from './math';
import type { User } from './types';

interface Greeter {
    greet(name: string): string;
}

class Person implements Greeter {
    private name: string;

    constructor(name: string) {
        this.name = name;
    }

    greet(person: string): string {
        return `Hello ${person}, I'm ${this.name}`;
    }
}

function makeGreeter(name: string): Greeter {
    return new Person(name);
}

const greet = (name: string): string => `Hi ${name}`;

const p = new Person("Alice");
p.greet("Bob");
makeGreeter("Charlie");
greet("Dave");
