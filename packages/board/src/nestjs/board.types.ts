export interface EddyqBoardOptions {
  /** URL path to mount the board at. Default: '/board' */
  mountPath?: string;
  auth?: {
    /** Default: 'admin' */
    username?: string;
    password: string;
  };
}
