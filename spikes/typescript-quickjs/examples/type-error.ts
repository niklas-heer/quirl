interface Command {
  program: string;
  args: string[];
  retries: number;
}

function validate(command: Command): number {
  return command.retries + 1;
}

const deploy: Command = {
  program: "deploy",
  args: ["--env", "production"],
  retries: "three",
};

validate(deploy);
