const child_process = require("child_process");
const os = require("os");

const aws = require("aws-sdk");
const fs = require("fs");

// {
//   "region": "us-east-1",
//   "networkOrRpc": "mainnet-beta",
//   "programName": "candy-machine",
//   "binaryS3Path": {
//     "bucket": "autodeploy-program-bucket",
//     "path": "dump.so"
//   }

// {
//   "region": "us-east-1",
//   "networkOrRpc": "mainnet-beta",
//   "programName": "candy-machine",
//   "binaryS3Path": {
//     "bucket": "some-bucket",
//     "path": "some-path"
//   },
//     "idlS3Path": {
//     "bucket": "some-bucket",
//     "path": "some-path"
//   },
// }

const DEPLOY_KP_PATH = "/tmp/deploy-keypair.json";
const PROGRAM_KP_PATH = "/tmp/program-keypair.json";
const EXECUTION_PATH = "/var/task";

const TESTNET = "testnet";
const DEVNET = "devnet";
const MAINNET_BETA = "mainnet-beta";

const VALID_NETWORKS = [TESTNET, DEVNET, MAINNET_BETA];

const NETWORK_TO_RPC = {
  [TESTNET]: "https://api.testnet.solana.com",
  [DEVNET]: "https://devnet.genesysgo.net/",
  [MAINNET_BETA]:
    "https://api.metaplex.solana.com/5ebea512d12be102f53d319dafc8",
};

const setSolanaConfig = (rpc, wallet) => {
  console.log("set default solana config");
  child_process.spawnSync(
    "solana",
    [
      "config",
      "set",
      "--url",
      rpc,
      "--keypair",
      wallet,
      "--commitment",
      "finalized",
    ],
    {
      env: process.env,
      stdio: "inherit",
    }
  );
};

const fetchSecretsValue = async (client, secretName) => {
  const params = {
    SecretId: secretName,
  };

  // https://docs.aws.amazon.com/secretsmanager/latest/apireference/API_GetSecretValue.html
  try {
    const data = await client.getSecretValue(params).promise();
    let secret;
    // decrypt secret using the associated KMS key, depending the secret being a string or binary.
    if ("SecretString" in data) {
      secret = data.SecretString;
    } else {
      const buffer = Buffer.from(data.SecretBinary, "base64");
      secret = buffer.toString("ascii");
    }
    return secret;
  } catch (err) {
    console.log(
      `failed to get secret with input ${params} and error = ${err.code}: `,
      err
    );
    // err.code:
    // - DecryptionFailureException
    //  - secrets manager can't decrypt the protected secret text using the provided KMS key.
    // -InternalServiceErrorException
    //  - error occurred on the server side.
    // - InvalidParameterException
    //  - you provided an invalid value for a parameter.
    // - InvalidRequestException
    //  - you provided a parameter value that is not valid for the current state of the resource.
    // - ResourceNotFoundException")
    //  - can't find the resource that you asked for.

    throw err;
  }
};

const getRequiredParamWithValidation = (event, key, callback) => {
  return getParamWithValidation(event, key, callback);
};

const getOptionalParamWithValidation = (event, key, callback) => {
  return getParamWithValidation(event, key, callback, false);
};

const getParamWithValidation = (event, key, callback, required = true) => {
  if (!event[key] && required) {
    throw new Error("Parameter is required: ", key);
  }

  const value = event[key];
  if (callback) {
    return callback(value);
  }

  return value;
};

const validateSourceRepository = (repo) => {
  const metaplexProgramLibrary =
    "https://github.com/metaplex-foundation/metaplex-program-library";

  if (!repo) return metaplexProgramLibrary;
  if (!repo.startsWith(metaplexProgramLibrary)) {
    throw new Error("Expected repo to be an instance of MPL or security fork");
  }

  return repo;
};

const validateNetwork = (network) => {
  if (network) {
    if (!network.includes("https://")) {
      return network;
    } else if (!VALID_NETWORKS.includes(network)) {
      return NETWORK_TO_RPC[network];
    }
  }

  return NETWORK_TO_RPC[DEVNET];
};

const parseBucketAndPath = (location) => {
  let bucket = undefined;
  let path = undefined;

  if (location) {
    bucket = location["bucket"];
    path = location["path"];
  }

  return {
    bucket,
    path,
  };
};

// program like `candy-machine` or `token-entangler`
const getDefaultSecretNameFor = (program, scope, ending) =>
  `${program.toUpperCase()}-${scope.toUpperCase()}`;
const getDefaultSecretNameKeys = (program, scope) =>
  `${program.toUpperCase()}-KEYS`;
const getDefaultSecretNameForAuthority = (program) =>
  getDefaultSecretNameFor(program, "authority");
const getDefaultSecretNameForProgram = (program) =>
  getDefaultSecretNameFor(program, "program");

// fetch together
const serializeProgramKeypairs = async (client, program) => {
  const name = getDefaultSecretNameKeys(program);
  console.log("secret name: ", name);

  const result = await fetchSecretsValue(client, name);
  const resultAsJson = JSON.parse(result);
  console.log("resultAsJson: ", resultAsJson);

  const programSecretKey = getDefaultSecretNameForProgram(program);
  console.log("programSecretKey: ", programSecretKey);
  const programKp = resultAsJson[programSecretKey];
  console.log("programKp: ", programKp);
  fs.writeFileSync(PROGRAM_KP_PATH, Buffer.from(programKp, "utf-8"));

  const authoritySecretKey = getDefaultSecretNameForAuthority(program);
  console.log("authoritySecretKey: ", authoritySecretKey);
  const authorityKp = resultAsJson[authoritySecretKey];
  console.log("authorityKp: ", authorityKp);
  fs.writeFileSync(DEPLOY_KP_PATH, Buffer.from(authorityKp, "utf-8"));
};

const tryDownloadBinary = async (s3, program, bucket, path) => {
  const localPath = `/tmp/${program}.so`;
  await tryDownloadObject(s3, bucket, path, localPath);

  return localPath;
};

const tryDownloadIdl = async (s3, program, bucket, path) => {
  const localPath = `/tmp/${program}.json`;
  if (bucket && path) {
    await tryDownloadObject(s3, bucket, path, localPath);
    return localPath;
  }

  return undefined;
};

const tryDownloadObject = async (s3, bucket, path, localPath) => {
  const params = {
    Bucket: bucket,
    Key: path,
  };

  try {
    const data = await s3.getObject(params).promise();
    fs.writeFileSync(localPath, data.Body);
  } catch (err) {
    throw new Error("Unable to fetch S3 object with params: ", params);
  }
};

// no retries in beginning
// store secret in secrets manager

// region // default, us-east-1
// github repo
// keypair
// program (either go to program or rust subdir)
// networkOrRpc, default devnet (e.g. devnet or https://api.devnet.solana.com)
exports.handler = async (event) => {
  const region = getOptionalParamWithValidation(event, "region", (value) =>
    value ? value : "us-east-1"
  );
  const smClient = new aws.SecretsManager({
    region,
  });
  const s3Client = new aws.S3({
    region,
  });

  const programName = getRequiredParamWithValidation(event, "programName");
  // binary { bucket, path }
  const { bucket: binaryBucket, path: binaryPath } =
    getRequiredParamWithValidation(event, "binaryS3Path", parseBucketAndPath);
  // (optional) idl { bucket, path }
  const { bucket: idlBucket, path: idlPath } = getOptionalParamWithValidation(
    event,
    "idlS3Path",
    parseBucketAndPath
  );

  console.log("binaryBucket: ", binaryBucket, "; binaryPath: ", binaryPath);
  console.log("idlBucket: ", idlBucket, "; idlPath: ", idlPath);

  const networkOrRpc = getOptionalParamWithValidation(
    event,
    "networkOrRpc",
    validateNetwork
  );

  console.log(
    "running deploy for ",
    programName,
    " on ",
    networkOrRpc,
    "executing in aws region: ",
    region
  );

  try {
    // load keypairs from secrets manager
    await serializeProgramKeypairs(smClient, programName);

    console.log("DEPLOY_KP_PATH exists? ", fs.existsSync(DEPLOY_KP_PATH));
    // console.log("DEPLOY_KP_PATH content:  ", fs.readFileSync(DEPLOY_KP_PATH).toString('utf-8'));
    console.log("PROGRAM_KP_PATH exists? ", fs.existsSync(PROGRAM_KP_PATH));
    // console.log("PROGRAM_KP_PATH content:  ", fs.readFileSync(PROGRAM_KP_PATH).toString('utf-8'));

    const result = child_process.spawnSync(
      "solana",
      ["address", "-k", PROGRAM_KP_PATH],
      {
        env: process.env,
        cwd: EXECUTION_PATH,
        stdio: "pipe",
      }
    );
    console.log("result: ", result);

    // fetch deployable assets from s3
    const localBinaryPath = await tryDownloadBinary(
      s3Client,
      programName,
      binaryBucket,
      binaryPath
    );
    console.log("localBinaryPath: ", localBinaryPath);
    // console.log("localBinaryPath content: ", fs.readFileSync(localBinaryPath).toString('utf-8'));

    const localIdlPath = await tryDownloadIdl(
      s3Client,
      programName,
      idlBucket,
      idlPath
    );
    console.log("localIdlPath: ", localIdlPath);
    // if (localIdlPath) {
    //   console.log("localIdlPath content: ", fs.readFileSync(localIdlPath).toString('utf-8'));
    // }

    // set solana config & perform deploy
    setSolanaConfig(networkOrRpc, DEPLOY_KP_PATH);
    deployProgram(PROGRAM_KP_PATH, DEPLOY_KP_PATH, localBinaryPath);
  } catch (e) {
    console.error("Unable to deploy program: ", programName, e);
  } finally {
    rmDeployKeypair();
  }

  const response = {
    statusCode: 200,
    body: JSON.stringify(`Successfully deployed ${programName}!`),
  };
  return response;
};

const rmDeployKeypair = () => {
  child_process.spawnSync("rm", [DEPLOY_KP_PATH], {
    env: process.env,
    stdio: "inherit",
  });
};

const getProgramBinaryName = (program) => {
  const _program = `${program.replace(/\-/g, "_")}.so`;
  console.log("_program: ", _program);

  return _program;
};

// solana -v program deploy /workspace/${_PROGRAM_NAME}.so -u https://devnet.genesysgo.net --program-id /workspace/program_id.json -k /workspace/deploy.json
const deployProgram = (programKp, authorityKp, path) => {
  console.log("deploy path: ", path);

  child_process.spawnSync(
    "solana",
    [
      "-v",
      "program",
      "deploy",
      "--keypair",
      authorityKp,
      "--program-id",
      programKp,
      path,
    ],
    {
      env: process.env,
      cwd: EXECUTION_PATH,
      stdio: "inherit",
    }
  );
};

// anchor idl upgrade -f /workspace/${_PROGRAM_NAME}.json -provider.cluster devnet --provider.wallet /workspace/deploy.json ${_PROGRAM_ID}
const upgradeIdl = (programKp, programId, idlPath, authorityKp) => {
  console.log("idlPath: ", idlPath);

  const result = child_process.spawnSync(
    "solana",
    ["address", "-k", programKp],
    {
      env: process.env,
      cwd: EXECUTION_PATH,
      stdio: "pipe",
    }
  );
  console.log("result.stdOut: ", result.stdOut);
  // // todo: parsing
  // const programId = result.stdOut;

  // child_process.spawnSync(
  //   "anchor",
  //   ["idl", "upgrade", "-f", idlPath, "--provider.wallet", authorityKp, programId],
  //   {
  //     env: process.env,
  //     cwd: EXECUTION_PATH,
  //     stdio: "inherit",
  //   }
  // );
};
