export const SignUpForm=props=>{
return React.createElement("p",{
class:"signup"},React.createElement(
"label",null,"Username: ",React.
createElement("input",{class:"us\
ername",type:"text"})),React.createElement(
"label",null,"Password: ",React.
createElement("input",{class:"pa\
ssword",type:"password"})),React.
createElement("div",{class:"prim\
ary disabled"},props.buttonText),
React.createElement("small",null,
"By signing up, you are agreeing\
 to our ",React.createElement("a",
{href:"/tos/"},"terms of service"),
"."))};
