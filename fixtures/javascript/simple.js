import { add } from './math.js';
import greet from './hello.js';

/**
 * A simple calculator class.
 */
class Calculator {
    constructor(initial) {
        this.value = initial;
    }

    add(n) {
        this.value += n;
    }
}

function multiply(a, b) {
    return a * b;
}

const divide = (a, b) => a / b;

const calc = new Calculator(10);
calc.add(5);
multiply(2, 3);
divide(10, 2);
console.log("done");
