type Label = string;

function score(xs: number[]): number {
  return xs.reduce((a: number, b: number) => a + b, 0);
}

const label: Label = "aegis/typescript:";
console.log(label, score([1, 2, 3, 5, 8]));
